---
name: docker
category: system
description: >
  Manage Docker containers, images, volumes, networks, and Compose stacks.
  Use when the user asks to build, run, stop, restart, remove, inspect, or debug
  containers; pull, push, tag, or delete images; manage volumes and networks;
  run docker compose services; view logs or resource usage; clean up unused
  objects; or troubleshoot container issues. Keywords: docker, container, image,
  dockerfile, compose, volume, network, registry, build, deploy.
license: MIT
compatibility: Requires docker CLI (v20+). Docker Compose V2 integrated as `docker compose`.
metadata:
  author: zeph
  version: "1.0"
---

# Docker Operations

## Container Lifecycle

### List containers

```bash
# Running containers
docker ps

# All containers (including stopped)
docker ps -a

# Quiet mode (IDs only)
docker ps -q

# Filter by status
docker ps -f "status=exited"

# Custom format
docker ps --format "table {{.ID}}\t{{.Names}}\t{{.Status}}\t{{.Ports}}"
```

### Create and run

```bash
# Run in foreground
docker run IMAGE

# Run detached with a name
docker run -d --name NAME IMAGE

# Run with port mapping (host:container)
docker run -d -p 8080:80 IMAGE

# Run with environment variables
docker run -d -e KEY=VALUE -e KEY2=VALUE2 IMAGE

# Run with env file
docker run -d --env-file .env IMAGE

# Run interactive shell
docker run -it IMAGE /bin/bash

# Run with auto-remove on exit
docker run --rm IMAGE

# Run with volume mount (bind mount)
docker run -d -v /host/path:/container/path IMAGE

# Run with named volume
docker run -d -v myvolume:/data IMAGE

# Run with read-only filesystem
docker run --read-only IMAGE

# Run with resource limits
docker run -d --memory=512m --cpus=1.5 IMAGE

# Run with restart policy
docker run -d --restart=unless-stopped IMAGE

# Run on a specific network
docker run -d --network=mynet IMAGE

# Run with health check
docker run -d \
  --health-cmd="curl -f http://localhost/ || exit 1" \
  --health-interval=30s \
  --health-timeout=10s \
  --health-retries=3 \
  IMAGE
```

### Stop, start, restart, remove

```bash
# Stop gracefully (SIGTERM, then SIGKILL after timeout)
docker stop CONTAINER

# Stop with custom timeout (seconds)
docker stop -t 30 CONTAINER

# Start a stopped container
docker start CONTAINER

# Restart
docker restart CONTAINER

# Kill immediately (SIGKILL)
docker kill CONTAINER

# Remove stopped container
docker rm CONTAINER

# Force remove running container
docker rm -f CONTAINER

# Remove all stopped containers
docker container prune -f
```

### Execute commands in running containers

```bash
# Run command
docker exec CONTAINER COMMAND

# Interactive shell
docker exec -it CONTAINER /bin/bash

# Run as specific user
docker exec -u root CONTAINER COMMAND

# Set environment variable
docker exec -e VAR=value CONTAINER COMMAND

# Set working directory
docker exec -w /app CONTAINER COMMAND
```

## Logs and Inspection

### Logs

```bash
# View all logs
docker logs CONTAINER

# Follow log output (stream)
docker logs -f CONTAINER

# Show last N lines
docker logs --tail 100 CONTAINER

# Show logs since timestamp
docker logs --since 2h CONTAINER

# Show timestamps
docker logs -t CONTAINER

# Combine: last 50 lines + follow
docker logs --tail 50 -f CONTAINER
```

### Inspect and monitor

```bash
# Full container metadata (JSON)
docker inspect CONTAINER

# Extract specific field
docker inspect --format '{{.State.Status}}' CONTAINER
docker inspect --format '{{.NetworkSettings.IPAddress}}' CONTAINER

# Real-time resource usage
docker stats

# Stats for specific containers (no stream)
docker stats --no-stream CONTAINER1 CONTAINER2

# Running processes inside container
docker top CONTAINER

# Port mappings
docker port CONTAINER

# Filesystem changes since creation
docker diff CONTAINER
```

## Image Management

### Build

```bash
# Build from Dockerfile in current directory
docker build -t NAME:TAG .

# Build with specific Dockerfile
docker build -f Dockerfile.prod -t NAME:TAG .

# Build with build arguments
docker build --build-arg VERSION=1.0 -t NAME:TAG .

# Build without cache
docker build --no-cache -t NAME:TAG .

# Build with target stage (multi-stage)
docker build --target builder -t NAME:builder .

# Build with platform
docker build --platform linux/amd64 -t NAME:TAG .
```

### Pull, push, tag

```bash
# Pull image
docker pull IMAGE:TAG

# Pull specific platform
docker pull --platform linux/arm64 IMAGE:TAG

# Tag image
docker tag SOURCE:TAG TARGET:TAG

# Push to registry
docker push IMAGE:TAG

# Login to registry
docker login REGISTRY

# Logout
docker logout REGISTRY
```

### List and remove images

```bash
# List images
docker images

# List with filter
docker images --filter "dangling=true"

# Remove image
docker rmi IMAGE

# Force remove
docker rmi -f IMAGE

# Remove all dangling images
docker image prune -f

# Remove all unused images (not just dangling)
docker image prune -a -f

# Image layer history
docker history IMAGE

# Image disk usage
docker image ls --format "{{.Repository}}:{{.Tag}}\t{{.Size}}"
```

## Volumes

```bash
# Create named volume
docker volume create VOLUME_NAME

# List volumes
docker volume ls

# Inspect volume
docker volume inspect VOLUME_NAME

# Remove volume
docker volume rm VOLUME_NAME

# Remove all unused volumes
docker volume prune -f

# Copy files from container
docker cp CONTAINER:/path/to/file ./local/path

# Copy files to container
docker cp ./local/file CONTAINER:/path/to/dest
```

## Networking

```bash
# List networks
docker network ls

# Create network
docker network create NETWORK_NAME

# Create with driver
docker network create --driver bridge NETWORK_NAME

# Create with subnet
docker network create --subnet=172.20.0.0/16 NETWORK_NAME

# Inspect network
docker network inspect NETWORK_NAME

# Connect running container to network
docker network connect NETWORK_NAME CONTAINER

# Disconnect
docker network disconnect NETWORK_NAME CONTAINER

# Remove network
docker network rm NETWORK_NAME

# Remove all unused networks
docker network prune -f
```

## Docker Compose

Compose V2 is a subcommand of `docker` (not a separate `docker-compose` binary).

### Service lifecycle

```bash
# Start all services (detached)
docker compose up -d

# Start specific services
docker compose up -d SERVICE1 SERVICE2

# Start with build (rebuild images first)
docker compose up -d --build

# Start with forced recreation
docker compose up -d --force-recreate

# Start with custom compose file
docker compose -f docker-compose.prod.yml up -d

# Stop and remove containers, networks
docker compose down

# Stop and remove including volumes
docker compose down -v

# Stop and remove including images
docker compose down --rmi all

# Stop services without removing
docker compose stop

# Start stopped services
docker compose start

# Restart services
docker compose restart

# Restart a single service
docker compose restart SERVICE
```

### Build, logs, status

```bash
# Build or rebuild services
docker compose build

# Build without cache
docker compose build --no-cache

# Build a single service
docker compose build SERVICE

# View logs
docker compose logs

# Follow logs for specific service
docker compose logs -f SERVICE

# Last N lines
docker compose logs --tail 50 SERVICE

# List running services
docker compose ps

# List all services (including stopped)
docker compose ps -a

# Execute command in running service
docker compose exec SERVICE COMMAND

# Interactive shell
docker compose exec SERVICE /bin/bash

# Run one-off command (starts new container)
docker compose run --rm SERVICE COMMAND

# Scale service
docker compose up -d --scale SERVICE=3

# Pull latest images
docker compose pull

# View service configuration
docker compose config
```

## Cleanup and System

```bash
# Disk usage summary
docker system df

# Detailed disk usage
docker system df -v

# Remove all unused data (containers, networks, images, cache)
docker system prune -f

# Include volumes in prune
docker system prune --volumes -f

# Remove all unused data including all unused images
docker system prune -a -f

# Prune build cache
docker builder prune -f

# Prune build cache older than 24h
docker builder prune --filter "until=24h" -f
```

## Dockerfile Health Check

```dockerfile
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
  CMD curl -f http://localhost:8080/health || exit 1
```

- `--interval`: Time between checks (default 30s)
- `--timeout`: Max time for a single check (default 30s)
- `--start-period`: Grace period during startup (default 0s)
- `--retries`: Consecutive failures to report unhealthy (default 3)

Check health status: `docker inspect --format '{{.State.Health.Status}}' CONTAINER`

## Troubleshooting Patterns

### Container will not start

```bash
# Check logs for crash reason
docker logs CONTAINER

# Check exit code
docker inspect --format '{{.State.ExitCode}}' CONTAINER

# Run interactively to debug
docker run -it --entrypoint /bin/sh IMAGE
```

### Out of disk space

```bash
# Check what is using space
docker system df -v

# Aggressive cleanup
docker system prune -a --volumes -f
```

### Network connectivity issues

```bash
# Check container network settings
docker inspect --format '{{json .NetworkSettings.Networks}}' CONTAINER

# Test connectivity from inside container
docker exec CONTAINER ping OTHER_HOST

# Check DNS resolution
docker exec CONTAINER nslookup SERVICE_NAME
```

### Performance: `docker stats --no-stream` and `docker inspect --format '{{.State.OOMKilled}}' CONTAINER`
