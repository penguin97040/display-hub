# A Debian Bookworm machine with systemd as PID 1, standing in for Raspberry Pi OS to test the
# installer and service. It is a container on x86-64, not a Pi: see run.sh.
FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
RUN apt-get update \
 && apt-get install -y --no-install-recommends systemd systemd-sysv dbus util-linux procps \
 && rm -rf /var/lib/apt/lists/* \
 && systemctl mask systemd-firstboot.service getty.target console-getty.service
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
