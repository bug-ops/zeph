FROM container-registry.oracle.com/os/oraclelinux:9-slim

ARG TARGETARCH

RUN microdnf update -y && \
    (microdnf module enable nodejs:25 -y 2>/dev/null || \
     microdnf module enable nodejs:24 -y 2>/dev/null || \
     microdnf module enable nodejs:22 -y 2>/dev/null || \
     microdnf module enable nodejs:20 -y) && \
    microdnf install -y \
    shadow-utils ca-certificates \
    curl wget git jq file findutils procps-ng \
    nodejs npm python3 && \
    microdnf clean all && \
    useradd --system --create-home --shell /sbin/nologin zeph

WORKDIR /app

COPY binaries/zeph-${TARGETARCH} /app/zeph
COPY config/ /app/config/
COPY skills/ /app/skills/

RUN mkdir -p /app/data && \
    chown -R zeph:zeph /app && \
    chmod +x /app/zeph

USER zeph

ENTRYPOINT ["/app/zeph"]
