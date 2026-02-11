FROM container-registry.oracle.com/os/oraclelinux:9-slim

ARG TARGETARCH

RUN microdnf update -y && \
    NODE_STREAM=$(microdnf module list nodejs 2>/dev/null | grep -oP '^\s*nodejs\s+\K\d+' | sort -rn | head -1) && \
    microdnf module enable "nodejs:${NODE_STREAM}" -y && \
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
