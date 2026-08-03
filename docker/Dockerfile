# Stage 1: prepare writable runtime directories with correct ownership.
# busybox is used only to run mkdir/chown; it is not present in the final image.
FROM busybox:1.38-musl@sha256:32b5cdad7cce41dfd53d0ae06baebcf8357a147ee7694dc706911c373bc30c37 AS setup

ARG TARGETARCH

COPY binaries/zeph-${TARGETARCH} /app/zeph
COPY config/ /app/config/
COPY .zeph/skills/ /app/.zeph/skills/

# distroless nonroot uid is 65532
RUN mkdir -p /app/.zeph/data && \
    chown -R 65532:65532 /app

# Stage 2: minimal distroless runtime — no shell, no package manager, zero CVEs.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:f5b485ea962d9bd1186b2f6b3a061191539b905b82ec395de78cbfae51f20e35

COPY --from=setup /app /app

WORKDIR /app

ENTRYPOINT ["/app/zeph"]
