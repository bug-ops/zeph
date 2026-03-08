---
name: docker
description: Manage Docker containers and images. Use when the user asks to build, run, stop, or inspect containers, view logs, or work with docker compose.
compatibility: Requires docker
---
# Docker Operations

## List containers
```bash
docker ps -a
```

## Build image
```bash
docker build -t NAME:TAG .
```

## Run container
```bash
docker run -d --name NAME IMAGE
```

## View logs
```bash
docker logs CONTAINER
```

## Compose
```bash
docker compose up -d
docker compose down
```
