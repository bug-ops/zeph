---
name: docker
description: Docker container operations - build, run, ps, logs, compose.
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
