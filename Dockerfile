FROM container-registry.oracle.com/os/oraclelinux:9-slim

ARG TARGETARCH

RUN microdnf install -y \
    shadow-utils ca-certificates \
    curl wget git jq file findutils procps-ng && \
    microdnf clean all && \
    useradd --system --no-create-home --shell /sbin/nologin zeph

WORKDIR /app

COPY binaries/zeph-${TARGETARCH} /app/zeph
COPY config/ /app/config/
COPY skills/ /app/skills/

RUN mkdir -p /app/data && \
    chown -R zeph:zeph /app && \
    chmod +x /app/zeph

USER zeph

ENTRYPOINT ["/app/zeph"]
