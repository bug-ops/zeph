# Stage 1: prepare writable runtime directories with correct ownership.
# busybox is used only to run mkdir/chown; it is not present in the final image.
FROM busybox:1.37-musl@sha256:222ad6d973c0d198014546a65cd02c5fdedcc172123c5b4c2bf0af636550bd94 AS setup

ARG TARGETARCH

COPY binaries/zeph-${TARGETARCH} /app/zeph
COPY config/ /app/config/
COPY .zeph/skills/ /app/.zeph/skills/

# distroless nonroot uid is 65532
RUN mkdir -p /app/.zeph/data && \
    chown -R 65532:65532 /app

# Stage 2: minimal distroless runtime — no shell, no package manager, zero CVEs.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:d093aa3e30dbadd3efe1310db061a14da60299baff8450a17fe0ccc514a16639

COPY --from=setup /app /app

WORKDIR /app

ENTRYPOINT ["/app/zeph"]
