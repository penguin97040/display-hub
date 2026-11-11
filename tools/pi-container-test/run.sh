#!/usr/bin/env bash
# Installs Display Hub in a Debian Bookworm container running systemd, then checks the service as a
# Pi would use it: install, health, a display enrolling and polling, restarts, a crash, a network
# cut, a reboot, a bad configuration, backup and restore, an upgrade and an uninstall.
#
# A CONTAINER, NOT A PI: x86-64, Docker's network, no SD card or power cuts. It tests the installer,
# the unit and the recovery steps; docs/deploy-raspberry-pi.md has what to check on the Pi itself.
#
#   cargo zigbuild --release --locked -p hubd -p display-simulator --target x86_64-unknown-linux-gnu.2.36
#   bash tools/pi-container-test/run.sh
#
# Needs Docker that can run a privileged container, and port 8443 free on 127.0.0.1.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
rel=${CARGO_TARGET_DIR:-$root/target}/x86_64-unknown-linux-gnu/release
name=display-hub-pi-test
work=$(mktemp -d)
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$work"; }
trap cleanup EXIT

failures=0
pass() { echo "ok   $*"; }
bad() { echo "FAIL $*"; failures=$((failures + 1)); }
check() { local what=$1; shift; if "$@"; then pass "$what"; else bad "$what"; fi; }
box() { docker exec "$name" "$@"; }
as_hub() { docker exec "$name" runuser -u display-hub -- "$@"; }
sim() { "$rel/display-simulator" "$@"; }
active() {
  for _ in $(seq 1 30); do
    box systemctl is-active --quiet display-hub && box test -f /var/lib/display-hub/identity/hub-cert.pem && return 0
    sleep 1
  done
  return 1
}
healthy() {
  for _ in $(seq 1 15); do
    curl -sk --noproxy '*' --max-time 3 https://127.0.0.1:8443/v1/health | grep -q '"status":"ok"' && return 0
    sleep 1
  done
  return 1
}
booted() {
  for _ in $(seq 1 30); do
    case $(box systemctl is-system-running 2>/dev/null || true) in running | degraded) return 0 ;; esac
    sleep 1
  done
  return 1
}
display_online() { sim once --state "$work/display" >/dev/null 2>&1; }
display_offline_with_data() { local s=0; sim once --state "$work/display" >/dev/null 2>&1 || s=$?; [ "$s" = 3 ]; }
fingerprint() { as_hub hubd fingerprint 2>/dev/null; }

for f in hubd display-simulator; do
  [ -x "$rel/$f" ] || { echo "no $rel/$f; build it first (see the top of this script)" >&2; exit 2; }
done
docker build -q -t "$name" "$root/tools/pi-container-test" >/dev/null
docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  --tmpfs /run --tmpfs /run/lock -p 127.0.0.1:8443:8443 "$name" >/dev/null
check "the container boots systemd" booted

# Install, as on the Pi: a folder with hubd and the deploy files.
box mkdir -p /root/display-hub
docker cp "$rel/hubd" "$name:/root/display-hub/hubd"
for f in install.sh display-hub.service hubd.toml nftables-display-hub.conf; do
  docker cp "$root/deploy/raspberry-pi/$f" "$name:/root/display-hub/$f"
done
out=$(box bash /root/display-hub/install.sh 2>&1) || { echo "$out"; bad "install.sh"; }
check "install.sh starts the service" active
check "the hub answers on 8443" healthy
check "the unit file verifies" box systemd-analyze verify /etc/systemd/system/display-hub.service
check "the data folder is private" bash -c "[ \"\$(docker exec $name stat -c '%a %U' /var/lib/display-hub)\" = '700 display-hub' ]"
check "the key is owner-only" bash -c "[ \"\$(docker exec $name stat -c '%a' /var/lib/display-hub/identity/hub-key.pem)\" = 600 ]"
score=$(box systemd-analyze security display-hub --no-pager 2>/dev/null | tail -1)
echo "     systemd-analyze security: $score"
exposure=$(grep -oE '[0-9]+\.[0-9]+' <<<"$score" | head -1)
check "the exposure score is low (under 2.0)" awk "BEGIN { exit !(${exposure:-9} < 2.0) }"
key=$(fingerprint)

# A display: a setup file made on the "Pi", enrolled by the simulator on this machine.
as_hub hubd display-bundle --name Desk --host 127.0.0.1 --out /var/lib/display-hub/setup.json >/dev/null
box cat /var/lib/display-hub/setup.json >"$work/setup.json"
box rm /var/lib/display-hub/setup.json
sim enrol --bundle "$work/setup.json" --state "$work/display" >/dev/null
check "a display enrols and polls" display_online

box systemctl restart display-hub
check "restarted: running again" active
check "restarted: same key" test "$(fingerprint)" = "$key"
check "restarted: the display still polls" display_online

box systemctl kill --kill-whom=main -s KILL display-hub
sleep 7
check "killed: systemd starts it again" active
check "killed: the display still polls" display_online

docker network disconnect bridge "$name"
check "network cut: the display shows its saved data" display_offline_with_data
docker network connect bridge "$name"
check "network back: the hub answers" healthy
check "network back: the display polls again" display_online

docker restart "$name" >/dev/null
check "rebooted: systemd is up" booted
check "rebooted: the service started by itself" active
check "rebooted: same key" test "$(fingerprint)" = "$key"
check "rebooted: the display's credential still works" display_online

box cp /etc/display-hub/hubd.toml /root/hubd.toml.good
box sh -c 'echo "refresh_after_seconds = 5" >> /etc/display-hub/hubd.toml'
box systemctl restart display-hub || true
sleep 3
check "a bad setting stops the service" bash -c "! docker exec $name systemctl is-active --quiet display-hub"
check "and the journal says why" bash -c "docker exec $name journalctl -u display-hub --no-pager | grep -q 'refresh_after_seconds'"
box cp /root/hubd.toml.good /etc/display-hub/hubd.toml
box systemctl restart display-hub
check "the fixed setting starts it again" active

box install -d -m 0700 -o display-hub -g display-hub /var/backups/display-hub
check "a backup while running" bash -c "docker exec $name runuser -u display-hub -- hubd backup /var/backups/display-hub/test >/dev/null"
check "restore is refused while running" bash -c "! docker exec $name runuser -u display-hub -- hubd restore /var/backups/display-hub/test --replace 2>/dev/null"
box systemctl stop display-hub
check "restore is refused without --replace" bash -c "! docker exec $name runuser -u display-hub -- hubd restore /var/backups/display-hub/test 2>/dev/null"
check "restore with --replace" bash -c "docker exec $name runuser -u display-hub -- hubd restore /var/backups/display-hub/test --replace >/dev/null"
check "the replaced hub was kept" bash -c "docker exec $name sh -c 'ls -d /var/lib/display-hub/replaced-*' >/dev/null"
box systemctl start display-hub
check "restored: running" active
check "restored: same key" test "$(fingerprint)" = "$key"
check "restored: the display still polls" display_online

out=$(box bash /root/display-hub/install.sh 2>&1) || bad "install.sh again"
check "upgrade: backed up first" grep -q "Backed up the current hub" <<<"$out"
check "upgrade: settings kept" grep -q "Kept your" <<<"$out"
check "upgrade: running, same key" bash -c "docker exec $name systemctl is-active --quiet display-hub && [ \"\$(docker exec $name runuser -u display-hub -- hubd fingerprint)\" = '$key' ]"

box bash /root/display-hub/install.sh --uninstall >/dev/null
check "uninstall: the service is gone" bash -c "! docker exec $name systemctl cat display-hub >/dev/null 2>&1"
check "uninstall: the data is kept" box test -f /var/lib/display-hub/identity/hub-key.pem

if [ "$failures" = 0 ]; then
  echo "Pi install test (container, not a Pi): all passed"
else
  echo "Pi install test (container, not a Pi): $failures failed"
  exit 1
fi
