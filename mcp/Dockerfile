# Build from the repository root so the whole workspace is in context:
#   docker build -f mcp/Dockerfile -t regional-mcp .
#
# Serve a different region by pointing at a different config at build time:
#   docker build -f mcp/Dockerfile --build-arg REGION_FILE=region.vermont.toml .
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
    cargo build --release --locked -p mcp && \
    cp /build/target/release/mcp /usr/local/bin/mcp

FROM debian:bookworm-slim

# Which region this image serves. Any TOML in the build context will do; see
# region.colorado.toml for the shape.
ARG REGION_FILE=region.colorado.toml

# `openssh-server`, `iproute2` and `socat` are not optional in a Firecracker
# guest: /init.sh brings eth0 up with `ip` from the kernel's `ip=` parameter,
# sshd is what backs `heyctl exec` and `heyctl shell`, and socat is what carries
# this VM's application logs off the box — the daemon starts
# `tail -F /var/log/heyvm-start.log | socat -u - VSOCK-CONNECT:…` inside the
# guest, and without socat that pump never runs and app-obs shows this
# deployment's control-plane lines and nothing the application printed.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl openssh-server iproute2 iputils-ping procps socat \
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

COPY --from=builder /usr/local/bin/mcp /usr/local/bin/mcp
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
    && echo 'regional-mcp' > /etc/heyo/hostname \
    && echo 'regional-mcp' > /etc/hostname

USER regional
ENV PORT=8080 \
    BIND_HOST=0.0.0.0 \
    REGION_CONFIG=/etc/regional/region.toml \
    RUST_LOG=info
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:${PORT}/healthz" || exit 1

ENTRYPOINT ["/usr/local/bin/mcp"]
