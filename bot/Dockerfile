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

# `openssh-server` and `iproute2` are not optional in a Firecracker guest:
# /init.sh brings eth0 up with `ip` from the kernel's `ip=` parameter, and sshd
# is what backs `heyctl exec` and `heyctl shell` into the VM. Without them the
# VM boots unreachable and undebuggable.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl openssh-server iproute2 iputils-ping procps \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home regional

# Host keys at build time so sshd does not block on entropy at first boot, and
# the login policy written explicitly because there is no cloud-init to fix it
# later. The guest is reachable only through the host's tap device.
RUN mkdir -p /run/sshd /etc/ssh/sshd_config.d \
    && echo "PermitRootLogin yes" >> /etc/ssh/sshd_config \
    && echo "PermitEmptyPasswords yes" >> /etc/ssh/sshd_config \
    && echo "PasswordAuthentication yes" > /etc/ssh/sshd_config.d/50-heyo.conf \
    && chmod 644 /etc/ssh/sshd_config.d/50-heyo.conf \
    && passwd -d root \
    && ssh-keygen -A

COPY --from=builder /usr/local/bin/bot /usr/local/bin/bot
# Baked in, because heyo's firecracker_containerd backend takes no --mount.
# REGION_CONFIG can still point elsewhere at runtime where a mount exists.
COPY ${REGION_FILE} /etc/regional/region.toml

# PID 1 for the microVM. The kernel is booted with `init=/init.sh`, so an image
# without this panics before userspace starts; see rootfs/init.sh. ENTRYPOINT
# below is what `docker compose up` uses and the kernel ignores — both paths
# work from one image.
COPY rootfs/init.sh /init.sh
RUN chmod +x /init.sh \
    && mkdir -p /etc/heyo \
    && echo 'regional-bot' > /etc/heyo/hostname \
    && echo 'regional-bot' > /etc/hostname

USER regional
ENV PORT=8081 \
    BIND_HOST=0.0.0.0 \
    REGION_CONFIG=/etc/regional/region.toml \
    RUST_LOG=info
EXPOSE 8081

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:${PORT}/healthz" || exit 1

ENTRYPOINT ["/usr/local/bin/bot"]
