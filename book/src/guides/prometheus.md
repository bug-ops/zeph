# Prometheus Monitoring

Zeph can expose a `/metrics` endpoint in [OpenMetrics](https://openmetrics.io/) format that
Prometheus can scrape. A pre-built Grafana dashboard is included for instant visualization.

## Prerequisites

- Zeph built with the `prometheus` feature (included in `server` and `full` feature sets)
- Docker (for the bundled Prometheus + Grafana stack)

## Enable the Metrics Endpoint

In your `config.toml`:

```toml
[gateway]
enabled = true
port = 8090

[metrics]
enabled = true
path = "/metrics"
sync_interval_secs = 5
```

The `prometheus` feature implies `gateway`, so you only need to enable the gateway once.

Verify the endpoint is live:

```bash
curl http://localhost:8090/metrics
```

You should see OpenMetrics text output ending with `# EOF`.

## Start the Monitoring Stack

```bash
docker compose -f docker/docker-compose.metrics.yml up
```

This starts:
- **Prometheus** on `http://localhost:9090` — scrapes Zeph every 10 seconds
- **Grafana** on `http://localhost:3000` — pre-configured with the Zeph dashboard

Both services include health checks; Grafana waits until Prometheus passes its health check before starting.

Open Grafana at `http://localhost:3000`. No login is required in the default configuration
(anonymous viewer access is enabled). The **Zeph Overview** dashboard is available under
**Dashboards → Zeph**.

### Custom Metrics Host

If Zeph listens on a different port, set `ZEPH_METRICS_HOST` before starting the stack.
You must also update `docker/prometheus/prometheus.yml` (`static_configs.targets`) to match,
since Prometheus does not expand environment variables in its scrape config:

```bash
# 1. Edit docker/prometheus/prometheus.yml targets to ["192.168.1.10:9000"]
# 2. Start the stack:
ZEPH_METRICS_HOST=192.168.1.10:9000 docker compose -f docker/docker-compose.metrics.yml up
```

### Linux Networking

`host.docker.internal` resolves automatically on Docker Desktop (macOS/Windows) and on
Docker Engine >= 20.10 with the `extra_hosts: host.docker.internal:host-gateway` entry already
set in `docker-compose.metrics.yml`. On older Linux setups, set `network_mode: host` on the
`prometheus` service in the compose file instead.

## Running Alongside the Docker Stack

If Zeph is running inside Docker (e.g. `docker-compose.yml`), add the metrics overlay:

```bash
docker compose -f docker/docker-compose.yml -f docker/docker-compose.metrics.yml up
```

Then edit `docker/prometheus/prometheus.yml` to scrape the Zeph container instead of the host:

```yaml
scrape_configs:
  - job_name: "zeph"
    static_configs:
      - targets: ["zeph:8090"]   # Docker service name
```

## Dashboard Panels

The **Zeph Overview** dashboard includes these panel rows:

| Row | Metrics |
|-----|---------|
| LLM Performance | Token rate, API call rate, last-call latency, context tokens |
| LLM Latency Histograms | p50/p95/p99 for LLM calls, turns, and tool executions |
| Agent Turn Phases | Last/average/max duration per phase (prepare_context, llm_chat, tool_exec, persist) |
| Memory & Context | Message count, embedding rate, compaction rate, Qdrant status |
| Tools & Cache | Cache hit/miss rate, tool output prune rate |
| Security | Injection flags, exfiltration blocks, quarantine invocations, rate-limit trips |
| System | Uptime, skills loaded, MCP server status, background task counts, orchestration rates |

## Custom Prometheus Configuration

If you already have a Prometheus instance, add Zeph as a scrape target:

```yaml
scrape_configs:
  - job_name: "zeph"
    static_configs:
      - targets: ["<zeph-host>:8090"]
    metrics_path: "/metrics"
    scrape_interval: 10s
```

Replace `<zeph-host>` with the hostname or IP where Zeph is running.

## Change the Admin Password

Set `GRAFANA_ADMIN_PASSWORD` before starting the stack:

```bash
GRAFANA_ADMIN_PASSWORD=mysecret docker compose -f docker/docker-compose.metrics.yml up
```

## Troubleshooting

**`curl http://localhost:8090/metrics` returns connection refused**

Check that both `[gateway] enabled = true` and `[metrics] enabled = true` are set in your config.
The gateway binds to `0.0.0.0:8090` by default.

**Prometheus shows `zeph` target as DOWN**

On Linux, `host.docker.internal` requires Docker Engine 20.10+ with `--add-host`.
If your setup doesn't support it, switch to `network_mode: host` in
`docker/docker-compose.metrics.yml` for the `prometheus` service, or use the container name
when Zeph runs inside Docker.

**No data in Grafana**

Confirm Prometheus can reach the metrics endpoint: open `http://localhost:9090/targets` and
check that `zeph` is in state UP. If the target is down, verify the `targets` in
`docker/prometheus/prometheus.yml` matches where Zeph is listening.
