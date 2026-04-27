# Stage 1: prepare writable runtime directories with correct ownership.
# busybox is used only to run mkdir/chown; it is not present in the final image.
FROM busybox:1.37-musl AS setup

ARG TARGETARCH

COPY binaries/zeph-${TARGETARCH} /app/zeph
COPY config/ /app/config/
COPY .zeph/skills/ /app/.zeph/skills/

# distroless nonroot uid is 65532
RUN mkdir -p /app/.zeph/data && \
    chown -R 65532:65532 /app

# Stage 2: minimal distroless runtime — no shell, no package manager, zero CVEs.
FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=setup /app /app

WORKDIR /app

ENTRYPOINT ["/app/zeph"]
