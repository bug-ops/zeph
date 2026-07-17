---
name: qdrant
category: data
description: >
  Manage Qdrant vector database via REST API. Use when the user asks to create
  or delete collections, upsert or search vectors, inspect points, filter by
  payload fields, manage snapshots, check cluster status, or debug semantic
  search issues. Covers collection CRUD, point upsert/search/scroll/count,
  payload indexes, filtering (must/should/must_not), recommend, discover,
  snapshots, aliases, and cluster operations.
compatibility: Requires curl and a running Qdrant instance (default http://localhost:6333)
metadata:
  author: zeph
  version: "1.0"
---

# Qdrant Vector Database

Base URL: `http://localhost:6333` (default). Override with `QDRANT_URL` env var.

All examples use curl. Replace `localhost:6333` with the actual Qdrant endpoint.

## Quick Reference

| Task | Endpoint |
|------|----------|
| List collections | `GET /collections` |
| Create collection | `PUT /collections/{name}` |
| Delete collection | `DELETE /collections/{name}` |
| Collection info | `GET /collections/{name}` |
| Upsert points | `PUT /collections/{name}/points` |
| Search | `POST /collections/{name}/points/search` |
| Scroll (paginate) | `POST /collections/{name}/points/scroll` |
| Get point | `GET /collections/{name}/points/{id}` |
| Delete points | `POST /collections/{name}/points/delete` |
| Count points | `POST /collections/{name}/points/count` |
| Health check | `GET /healthz` |

## Collections

### List all collections

```bash
curl -s http://localhost:6333/collections | jq
```

### Create collection

```bash
curl -s -X PUT http://localhost:6333/collections/my_collection \
  -H "Content-Type: application/json" \
  -d '{
    "vectors": {
      "size": 384,
      "distance": "Cosine"
    }
  }' | jq
```

Distance metrics: `Cosine`, `Euclid`, `Dot`, `Manhattan`.

### Create collection with named vectors

```bash
curl -s -X PUT http://localhost:6333/collections/my_collection \
  -H "Content-Type: application/json" \
  -d '{
    "vectors": {
      "content": {"size": 384, "distance": "Cosine"},
      "title": {"size": 128, "distance": "Cosine"}
    }
  }' | jq
```

### Collection info

```bash
curl -s http://localhost:6333/collections/my_collection | jq
```

Key fields: `vectors_count`, `points_count`, `segments_count`, `status` (green/yellow/red).

### Delete collection

```bash
curl -s -X DELETE http://localhost:6333/collections/my_collection | jq
```

### Check collection exists

```bash
curl -sI http://localhost:6333/collections/my_collection -o /dev/null -w "%{http_code}"
# 200 = exists, 404 = not found
```

## Points

### Upsert points (insert or update)

```bash
curl -s -X PUT http://localhost:6333/collections/my_collection/points \
  -H "Content-Type: application/json" \
  -d '{
    "points": [
      {
        "id": 1,
        "vector": [0.1, 0.2, 0.3, ...],
        "payload": {"title": "Example", "category": "test", "score": 0.95}
      },
      {
        "id": 2,
        "vector": [0.4, 0.5, 0.6, ...],
        "payload": {"title": "Another", "category": "prod", "score": 0.87}
      }
    ]
  }' | jq
```

Point IDs can be integers or UUIDs.

### Upsert with named vectors

```bash
curl -s -X PUT http://localhost:6333/collections/my_collection/points \
  -H "Content-Type: application/json" \
  -d '{
    "points": [
      {
        "id": 1,
        "vector": {
          "content": [0.1, 0.2, ...],
          "title": [0.3, 0.4, ...]
        },
        "payload": {"title": "Example"}
      }
    ]
  }' | jq
```

### Get point by ID

```bash
curl -s http://localhost:6333/collections/my_collection/points/1 | jq
```

### Get multiple points

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points \
  -H "Content-Type: application/json" \
  -d '{"ids": [1, 2, 3]}' | jq
```

### Delete points by ID

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points/delete \
  -H "Content-Type: application/json" \
  -d '{"points": [1, 2, 3]}' | jq
```

### Delete points by filter

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points/delete \
  -H "Content-Type: application/json" \
  -d '{
    "filter": {
      "must": [
        {"key": "category", "match": {"value": "obsolete"}}
      ]
    }
  }' | jq
```

### Count points

```bash
# Total count
curl -s -X POST http://localhost:6333/collections/my_collection/points/count \
  -H "Content-Type: application/json" \
  -d '{}' | jq '.result.count'

# Count with filter
curl -s -X POST http://localhost:6333/collections/my_collection/points/count \
  -H "Content-Type: application/json" \
  -d '{
    "filter": {
      "must": [{"key": "category", "match": {"value": "test"}}]
    },
    "exact": true
  }' | jq '.result.count'
```

## Search

### Basic vector search

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points/search \
  -H "Content-Type: application/json" \
  -d '{
    "vector": [0.1, 0.2, 0.3, ...],
    "limit": 10,
    "with_payload": true,
    "with_vector": false
  }' | jq
```

### Search with filter

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points/search \
  -H "Content-Type: application/json" \
  -d '{
    "vector": [0.1, 0.2, 0.3, ...],
    "limit": 5,
    "with_payload": true,
    "filter": {
      "must": [
        {"key": "category", "match": {"value": "prod"}}
      ],
      "must_not": [
        {"key": "archived", "match": {"value": true}}
      ]
    }
  }' | jq
```

### Search with score threshold

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points/search \
  -H "Content-Type: application/json" \
  -d '{
    "vector": [0.1, 0.2, 0.3, ...],
    "limit": 10,
    "score_threshold": 0.7,
    "with_payload": true
  }' | jq
```

### Search with named vector

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points/search \
  -H "Content-Type: application/json" \
  -d '{
    "vector": {
      "name": "content",
      "vector": [0.1, 0.2, 0.3, ...]
    },
    "limit": 10,
    "with_payload": true
  }' | jq
```

### Scroll (paginate through all points)

```bash
# First page
curl -s -X POST http://localhost:6333/collections/my_collection/points/scroll \
  -H "Content-Type: application/json" \
  -d '{
    "limit": 100,
    "with_payload": true,
    "with_vector": false
  }' | jq

# Next page (use next_page_offset from previous response)
curl -s -X POST http://localhost:6333/collections/my_collection/points/scroll \
  -H "Content-Type: application/json" \
  -d '{
    "limit": 100,
    "offset": NEXT_PAGE_OFFSET,
    "with_payload": true
  }' | jq
```

### Recommend (find similar to existing points)

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points/recommend \
  -H "Content-Type: application/json" \
  -d '{
    "positive": [1, 2],
    "negative": [3],
    "limit": 10,
    "with_payload": true
  }' | jq
```

## Filtering

Filters use `must` (AND), `should` (OR), `must_not` (NOT) clauses.

### Condition types

```json
// Exact match
{"key": "city", "match": {"value": "Berlin"}}

// Match any
{"key": "color", "match": {"any": ["red", "blue", "green"]}}

// Match except
{"key": "status", "match": {"except": ["deleted", "archived"]}}

// Range (numeric)
{"key": "price", "range": {"gte": 10.0, "lte": 100.0}}

// Range (dates as strings)
{"key": "created_at", "range": {"gte": "2025-01-01T00:00:00Z"}}

// Geo bounding box
{"key": "location", "geo_bounding_box": {
  "top_left": {"lat": 52.52, "lon": 13.38},
  "bottom_right": {"lat": 52.50, "lon": 13.42}
}}

// Geo radius
{"key": "location", "geo_radius": {
  "center": {"lat": 52.52, "lon": 13.405},
  "radius": 1000.0
}}

// Has key (field exists)
{"has_id": [1, 2, 3]}

// Is empty (field missing or null)
{"is_empty": {"key": "optional_field"}}

// Is null
{"is_null": {"key": "nullable_field"}}

// Nested filter
{"nested": {"key": "tags", "filter": {
  "must": [{"key": "tags[].name", "match": {"value": "important"}}]
}}}

// Values count
{"values_count": {"key": "tags", "gte": 2}}
```

### Combined filter example

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points/search \
  -H "Content-Type: application/json" \
  -d '{
    "vector": [0.1, 0.2, ...],
    "limit": 10,
    "filter": {
      "must": [
        {"key": "status", "match": {"value": "active"}},
        {"key": "score", "range": {"gte": 0.5}}
      ],
      "should": [
        {"key": "category", "match": {"value": "A"}},
        {"key": "category", "match": {"value": "B"}}
      ],
      "must_not": [
        {"is_empty": {"key": "embedding"}}
      ]
    }
  }' | jq
```

## Payload Indexes

Create indexes on payload fields for faster filtering.

```bash
# Create keyword index
curl -s -X PUT http://localhost:6333/collections/my_collection/index \
  -H "Content-Type: application/json" \
  -d '{"field_name": "category", "field_schema": "keyword"}' | jq

# Create integer index
curl -s -X PUT http://localhost:6333/collections/my_collection/index \
  -H "Content-Type: application/json" \
  -d '{"field_name": "score", "field_schema": "integer"}' | jq

# Create float index
curl -s -X PUT http://localhost:6333/collections/my_collection/index \
  -H "Content-Type: application/json" \
  -d '{"field_name": "price", "field_schema": "float"}' | jq

# Delete index
curl -s -X DELETE http://localhost:6333/collections/my_collection/index/category | jq
```

Index types: `keyword`, `integer`, `float`, `bool`, `geo`, `datetime`, `text`, `uuid`.

## Payload Operations

### Set payload on existing points

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points/payload \
  -H "Content-Type: application/json" \
  -d '{
    "payload": {"new_field": "value", "score": 0.99},
    "points": [1, 2, 3]
  }' | jq
```

### Delete payload keys

```bash
curl -s -X POST http://localhost:6333/collections/my_collection/points/payload/delete \
  -H "Content-Type: application/json" \
  -d '{
    "keys": ["old_field", "deprecated"],
    "points": [1, 2, 3]
  }' | jq
```

## Snapshots

```bash
curl -s http://localhost:6333/collections/my_collection/snapshots | jq       # list
curl -s -X POST http://localhost:6333/collections/my_collection/snapshots | jq  # create
curl -s -o snap.tar http://localhost:6333/collections/my_collection/snapshots/NAME  # download
curl -s -X DELETE http://localhost:6333/collections/my_collection/snapshots/NAME | jq  # delete
```

## Aliases

```bash
# Create/swap alias (atomic)
curl -s -X POST http://localhost:6333/aliases \
  -H "Content-Type: application/json" \
  -d '{"actions": [{"create_alias": {"alias_name": "prod", "collection_name": "v2"}}]}' | jq

# List aliases
curl -s http://localhost:6333/aliases | jq
```

## Service & Health

```bash
# Health check
curl -s http://localhost:6333/healthz

# Instance info
curl -s http://localhost:6333/ | jq

# Cluster info
curl -s http://localhost:6333/cluster | jq

# Telemetry
curl -s http://localhost:6333/telemetry | jq

# Prometheus metrics
curl -s http://localhost:6333/metrics
```

## Common Workflows

### Inspect collection health

```bash
# 1. Check collection status
curl -s http://localhost:6333/collections/my_collection | jq '.result.status, .result.points_count, .result.vectors_count'

# 2. Check optimizer status
curl -s http://localhost:6333/collections/my_collection | jq '.result.optimizer_status'

# 3. Check segment count (high count = needs optimization)
curl -s http://localhost:6333/collections/my_collection | jq '.result.segments_count'
```

### Debug search quality

```bash
# Search with vectors returned for inspection
curl -s -X POST http://localhost:6333/collections/my_collection/points/search \
  -H "Content-Type: application/json" \
  -d '{
    "vector": [0.1, 0.2, ...],
    "limit": 5,
    "with_payload": true,
    "with_vector": true
  }' | jq '.result[] | {id, score, payload}'
```

## Important Notes

- Qdrant default ports: 6333 (HTTP), 6334 (gRPC)
- Point IDs are either unsigned integers or UUIDs (string format)
- Vectors must match the collection's configured dimension exactly
- `with_payload: true` returns all payload; use `with_payload: ["field1", "field2"]` for specific fields
- Filters on non-indexed fields cause full scan — create payload indexes for fields used in filters
- Collection status: `green` = fully optimized, `yellow` = optimizing, `red` = error
- Upsert is idempotent — re-upserting a point with the same ID overwrites it
- Scroll with `offset: null` starts from the beginning
- Batch operations are more efficient than individual requests
