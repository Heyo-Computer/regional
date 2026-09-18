# Build from the repository root so the whole workspace is in context:
#   docker build -f bot/Dockerfile -t regional-bot .
#
# Serve a different region by pointing at a different config at build time:
#   docker build -f bot/Dockerfile --build-arg REGION_FILE=region.vermont.toml .
#
# The resulting OCI image is also the heyo VM image: the
# firecracker_containerd backend pulls ordinary registry references and
# boots them as Firecracker microVMs.

FROM rust:1.95-bookworm AS builder
WORKDIR /build

# Every workspace member has to be present for cargo to resolve the
# workspace, even when only one of them is being built.
COPY Cargo.toml Cargo.lock ./
COPY core core
COPY mcp mcp
COPY bot bot
COPY dashboard dashboard

# BuildKit cache mounts keep the registry and target directory warm between
# builds. The binary has to be copied out of the cache mount here, because a
# cache mount is not part of the layer the next stage copies from.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --locked -p bot && \
    cp /build/target/release/bot /usr/local/bin/bot

FROM debian:bookworm-slim

# Which region this image serves. Any TOML in the build context will do; see
# region.colorado.toml for the shape.
ARG REGION_FILE=region.colorado.toml

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home regional

COPY --from=builder /usr/local/bin/bot /usr/local/bin/bot
# Baked in, because heyo's firecracker_containerd backend takes no --mount.
# REGION_CONFIG can still point elsewhere at runtime where a mount exists.
COPY ${REGION_FILE} /etc/regional/region.toml

USER regional
ENV PORT=8081 \
    BIND_HOST=0.0.0.0 \
    REGION_CONFIG=/etc/regional/region.toml \
    RUST_LOG=info
EXPOSE 8081

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:${PORT}/healthz" || exit 1

ENTRYPOINT ["/usr/local/bin/bot"]
