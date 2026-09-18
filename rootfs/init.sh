#!/bin/sh
# PID 1 for the regional Firecracker microVM images.
#
# Vendored from heyo.git infra/firecracker/rootfs/init.sh — keep it close to
# that original so upstream fixes stay diffable.
#
# Firecracker boots the kernel straight into this script (`init=/init.sh` is on
# the kernel command line, and the host builds that line, not the image): there
# is no systemd, no cloud-init and no container runtime, so everything an init
# would normally do happens here — mount the virtual filesystems, bring up
# eth0, start sshd, print the ready marker and then stay alive.
#
# An image without this file does not "fail to start the app": the kernel
# panics before userspace exists, with
#
#     Kernel panic - not syncing: Requested init /init.sh failed (error -2)
#
# which is what every regional image did until this was added. The Dockerfile's
# ENTRYPOINT has nothing to do with it — that is Docker metadata, and the
# kernel never reads it. ENTRYPOINT is still what `docker compose up` uses, so
# both ways of running these images keep working.
#
# The `heyo-service` block below is inert here: these images ship no
# /etc/heyo/service.conf, because app-lb's `start_command` launches the app
# once the host has seen HEYVM_READY. It is kept so this file stays a small
# diff against upstream.
#
# Host contract (mvm-ctrl):
#   * `HEYVM_READY` must be printed on the serial console once the VM is up.
#     The host blocks on that exact string, and stray output corrupts the
#     marker-delimited serial protocol — service output belongs in log files.
#   * PID 1 must never exit; that panics the kernel.

set -u

mount -t proc proc /proc 2>/dev/null
mount -t sysfs sysfs /sys 2>/dev/null

# A Docker-exported rootfs has an empty /dev. Without device nodes sshd (and
# most of userspace) fails to start, so fall back to mknod when the kernel was
# built without devtmpfs.
mount -t devtmpfs devtmpfs /dev 2>/dev/null
if [ ! -c /dev/null ]; then
    echo "init: devtmpfs unavailable, creating device nodes manually"
    mknod -m 666 /dev/null    c 1 3
    mknod -m 666 /dev/zero    c 1 5
    mknod -m 444 /dev/random  c 1 8
    mknod -m 444 /dev/urandom c 1 9
    mknod -m 666 /dev/tty     c 5 0
    mknod -m 666 /dev/ptmx    c 5 2
    ln -sf /proc/self/fd /dev/fd
fi
mkdir -p /dev/pts && mount -t devpts devpts /dev/pts 2>/dev/null
mkdir -p /dev/shm && mount -t tmpfs tmpfs /dev/shm 2>/dev/null
mkdir -p /run && mount -t tmpfs tmpfs /run 2>/dev/null
mkdir -p /var/log

# Kernel chatter on ttyS0 interleaves with the serial command protocol.
dmesg -n 1 2>/dev/null

# Hostname. /etc/hostname is a runtime bind mount under Docker and exports as
# an empty file, so the image writes /etc/heyo/hostname as well and that is
# what is trusted first. Without this the kernel's `ip=` handler leaves the
# guest named after its own IP address.
HOSTNAME_FILE=/etc/heyo/hostname
[ -s "$HOSTNAME_FILE" ] || HOSTNAME_FILE=/etc/hostname
if [ -s "$HOSTNAME_FILE" ]; then
    GUEST_HOSTNAME=$(cat "$HOSTNAME_FILE")
    hostname "$GUEST_HOSTNAME" 2>/dev/null || \
        echo "$GUEST_HOSTNAME" > /proc/sys/kernel/hostname 2>/dev/null
fi

# /etc/hosts is another Docker runtime bind mount that exports empty. Without
# it `localhost` is not resolvable, so every service that talks to a local
# dependency falls through to DNS and stalls on a resolver timeout.
if ! grep -qs '127\.0\.0\.1' /etc/hosts; then
    {
        echo "127.0.0.1	localhost"
        echo "::1	localhost ip6-localhost ip6-loopback"
        [ -n "${GUEST_HOSTNAME:-}" ] && echo "127.0.1.1	$GUEST_HOSTNAME"
    } > /etc/hosts
fi

# Resolvers: an operator-supplied /etc/heyo/resolv.conf wins (private DNS,
# split horizon); otherwise fall back to a public resolver.
if [ -s /etc/heyo/resolv.conf ]; then
    cp /etc/heyo/resolv.conf /etc/resolv.conf
elif ! grep -qs '^nameserver' /etc/resolv.conf; then
    echo "nameserver 8.8.8.8" > /etc/resolv.conf
fi

# The kernel `ip=` parameter usually configures eth0, but it can lose the race
# with init, so re-apply it from /proc/cmdline when the address is missing.
ip link set lo up 2>/dev/null
ip link set eth0 up 2>/dev/null
if ! ip addr show eth0 2>/dev/null | grep -q "inet "; then
    for param in $(cat /proc/cmdline); do
        case "$param" in
            ip=*)
                GUEST_IP="${param#ip=}"; GUEST_IP="${GUEST_IP%%::*}"
                TAIL="${param#*::}"; GW="${TAIL%%:*}"
                ip addr add "$GUEST_IP/30" dev eth0 2>/dev/null
                [ -n "$GW" ] && ip route add default via "$GW" dev eth0 2>/dev/null
                ;;
        esac
    done
fi

# sshd backs `heyvm exec` and `heyvm sh`. Foreground mode (-D) so it does not
# double-fork, errors to a file so they never reach the serial console.
mkdir -p /run/sshd
chmod 755 /run/sshd
chown root:root /etc/ssh/ssh_host_* 2>/dev/null
chmod 600 /etc/ssh/ssh_host_*_key 2>/dev/null
chmod 644 /etc/ssh/ssh_host_*_key.pub 2>/dev/null
/usr/sbin/sshd -D -e </dev/null 2>/var/log/sshd.log &

# Application service. Deliberately not fatal: a VM that cannot reach its
# database should still boot far enough to log into and debug.
if [ -f /etc/heyo/service.conf ]; then
    /usr/local/bin/heyo-service start </dev/null >/dev/null 2>&1 || \
        echo "init: heyo-service failed to start, see /var/log/heyo-*.log"
fi

echo "HEYVM_READY"

# Serial console shell. The loop keeps PID 1 alive across `exit` / Ctrl-D —
# PID 1 exiting panics the kernel.
#
# It must be `sh`, not `bash`. `heyvm exec` drives this same console with
# marker-delimited commands and parses the lines between the markers; an
# interactive bash mangles that stream (readline echo/redraw on a serial line
# that reports a 0x0 window) and every exec times out after 30s. Measured on
# this image: bash login shell — exec always times out, `--noediting` included;
# dash — exec returns correctly. The stock `debian` image has the bash form and
# the same broken exec, so do not "restore" it here. Bash is still one word
# away for a human: type `bash` on the console, and SSH logins get it directly.
stty rows 50 cols 200 2>/dev/null
export PS1="${GUEST_HOSTNAME:-heyvm}:# "
while :; do /bin/sh; sleep 0.1; done
