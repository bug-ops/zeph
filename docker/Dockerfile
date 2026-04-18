FROM debian:bookworm-slim@sha256:4724b8cc51e33e398f0e2e15e18d5ec2851ff0c2280647e1310bc1642182655d

ARG TARGETARCH

RUN apt-get update && apt-get upgrade -y && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl wget git jq file findutils iproute2 procps \
    nodejs npm python3 && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/* && \
    useradd --system --create-home --shell /sbin/nologin zeph

WORKDIR /app

COPY binaries/zeph-${TARGETARCH} /app/zeph
COPY config/ /app/config/
COPY .zeph/skills/ /app/.zeph/skills/

RUN mkdir -p /app/.zeph/data && \
    chown -R zeph:zeph /app && \
    chmod +x /app/zeph

USER zeph

ENTRYPOINT ["/app/zeph"]
