#!/usr/bin/env bash
# Installs Display Hub in a Debian container running systemd (Bookworm or Trixie, the releases
# Raspberry Pi OS is built on), then checks the service as a Pi would use it: install, health, a
# display enrolling and polling, restarts, a crash, a network cut, a reboot, a bad configuration,
# backup and restore, helpers in their sandboxes, an upgrade from the release download (made by
# tools/release/pi-tarball.sh, unpacked in a throwaway folder as the one-line install does) and an
# uninstall.
#
# A CONTAINER, NOT A PI: x86-64, Docker's network, no SD card or power cuts. It tests the installer,
# the units and the recovery steps; docs/deploy-raspberry-pi.md has what to check on the Pi itself.
#
#   cargo zigbuild --release --locked -p hubd -p hub-send -p display-simulator --target x86_64-unknown-linux-gnu.2.36
#   bash tools/pi-container-test/run.sh            (Bookworm)
#   bash tools/pi-container-test/run.sh trixie
#
# Needs Docker that can run a privileged container, curl and python3, and port 8443 free on
# 127.0.0.1.
set -euo pipefail

suite=${1:-bookworm}
# Debian's official images, pinned by digest.
case $suite in
  bookworm) base=debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc ;;
  trixie) base=debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2 ;;
  *) echo "usage: $0 [bookworm|trixie]" >&2; exit 2 ;;
esac
root=$(cd "$(dirname "$0")/../.." && pwd)
rel=$(realpath -m "${CARGO_TARGET_DIR:-$root/target}")/x86_64-unknown-linux-gnu/release
name=display-hub-pi-test-$suite
# The highest exposure systemd-analyze may give a helper's unit. Lower than this needs cutting
# helpers off the internet, which is their job.
helper_max=1.5
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
# What install.sh keeps after the folder it ran from has gone.
share=/usr/local/share/display-hub
# "<mode> <owner>:<group>" of a file in the container.
mode_owner() { box stat -c '%a %U:%G' "$1"; }
is_mode() { [ "$(mode_owner "$2")" = "$1" ]; }
# The firewall settings, with the line docs/deploy-raspberry-pi.md says to add, load cleanly
# (checked only, not applied, so Docker's own network is left alone).
firewall_loads() { box nft -c -f /etc/nftables.conf; }

for f in hubd hub-send display-simulator; do
  [ -x "$rel/$f" ] || { echo "no $rel/$f; build it first (see the top of this script)" >&2; exit 2; }
done
echo "Debian $suite ($base)"
docker build -q --build-arg "BASE=$base" -t "$name" "$root/tools/pi-container-test" >/dev/null
docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  --tmpfs /run --tmpfs /run/lock -p 127.0.0.1:8443:8443 "$name" >/dev/null
check "the container boots systemd" booted

# Install, as on the Pi: a folder with hubd, hub-send and the deploy files.
docker cp "$root/deploy/raspberry-pi" "$name:/root/display-hub"
docker cp "$rel/hubd" "$name:/root/display-hub/hubd"
docker cp "$rel/hub-send" "$name:/root/display-hub/hub-send"
docker cp "$root/tools/pi-container-test/probe-helper" "$name:/root/display-hub/probe-helper"
# As if copied from Windows or downloaded from CI: no execute bits. install.sh copes.
box chmod 0644 /root/display-hub/hubd /root/display-hub/hub-send /root/display-hub/display-hub-helper /root/display-hub/helpers/pi-status
out=$(box bash /root/display-hub/install.sh 2>&1) || { echo "$out"; bad "install.sh"; }
check "install.sh starts the service" active
install_pairing=$(grep -oE 'dh1\.[A-Za-z0-9_-]+\.[0-9a-f]{8}' <<<"$out" | head -1 || true)
check "install.sh prints a pairing string on a first install" test -n "$install_pairing"
check "install.sh keeps its folder in $share" is_mode "755 root:root" "$share"
check "install.sh keeps a copy of itself (0755, root's)" is_mode "755 root:root" "$share/install.sh"
check "install.sh keeps the firewall rule (0644, root's)" is_mode "644 root:root" "$share/nftables-display-hub.conf"
check "the kept copies match the folder's" box sh -c "cmp -s /root/display-hub/install.sh $share/install.sh && cmp -s /root/display-hub/nftables-display-hub.conf $share/nftables-display-hub.conf"
box sh -c "echo 'include \"$share/nftables-display-hub.conf\"' >> /etc/nftables.conf"
check "the firewall includes the kept rule, as the guide says" firewall_loads
hub=https://127.0.0.1:8443
# install.sh only waits for systemd to say "active", so wait for the hub to answer (healthy retries for
# 15 seconds) before pairing with it.
check "the hub answers on 8443" healthy
# The manager's side: the string install.sh printed must really work, and at once (it expires).
pairing=$install_pairing
secret=$(python3 -c 'import base64, sys
p = sys.argv[1].split(".")[1]
b = base64.urlsafe_b64decode(p + "=" * (-len(p) % 4))
print(base64.urlsafe_b64encode(b[34:50]).decode().rstrip("="))' "$pairing")
admin=$(curl -sk --noproxy '*' --max-time 10 -H 'Content-Type: application/json' \
  -d "{\"secret\": \"$secret\", \"label\": \"container test\"}" "$hub/v1/admin/pair" |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["admin_token"])')
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

# Helpers. The manager's side first, over the admin API: pages fed by the example helper and by a
# probe helper that tries to get out of its sandbox.
hub=https://127.0.0.1:8443
api() { curl -sk --noproxy '*' --max-time 10 -H "Authorization: Bearer $admin" -H 'Content-Type: application/json' "$@"; }
api -o /dev/null -d '{"title": "Pi status", "kind": "big_number", "source": "helper", "feed": "pi-status", "id": "pi-status"}' "$hub/v1/admin/pages"
api -o /dev/null -d '{"title": "Probe", "kind": "text", "source": "helper", "feed": "probe", "id": "probe"}' "$hub/v1/admin/pages"
# A page's data field and when it was last updated, as "<value> <time>".
page_field() {
  api "$hub/v1/admin/pages/$1" | python3 -c "import json, sys
c = json.load(sys.stdin).get('content') or {}
print((c.get('data') or {}).get('$2'), c.get('updated_at'))"
}
page_matches() { local o; o=$(page_field "$1" "$2"); grep -qE "$3" <<<"$o"; }
pistate=/var/lib/private/display-hub-helper-pi-status
probestate=/var/lib/private/display-hub-helper-probe
helper_result() { [ "$(box systemctl show -p Result --value "display-hub-helper@$1.service")" = success ]; }
# Output is captured before grep reads it: with pipefail, grep -q stopping early would fail the pipe.
listed() { local o; o=$(box display-hub-helper list); grep -qE "$1" <<<"$o"; }
logged() { local o; o=$(box display-hub-helper logs "$1"); grep -q "$2" <<<"$o"; }
in_dynamic_range() { [ "$1" = "$2" ] && [ "$1" -ge 61184 ] && [ "$1" -le 65519 ]; }
refused() { ! box "$@" >/dev/null 2>&1; }
gone() { ! box test -e "$1"; }
not_in_journal() { local o; o=$(box journalctl --no-pager -o cat); ! grep -q "$1" <<<"$o"; }
not_in_folder() { local o; o=$(box grep -rl "$1" "$2" || true); [ -z "$o" ]; }

check "helper: add refuses a bad name" refused display-hub-helper add Bad_Name --run /bin/true
check "helper: add refuses a bad interval" refused display-hub-helper add bad --run /bin/true --every 10s
out=$(box display-hub-helper add pi-status --run /usr/local/share/display-hub/helpers/pi-status 2>&1) || { echo "$out"; bad "helper: add pi-status"; }
check "helper: add says it ran" grep -q "It ran without errors" <<<"$out"
check "helper: its timer is on" box systemctl is-active --quiet display-hub-helper@pi-status.timer
check "helper: its timer starts at boot" box systemctl is-enabled --quiet display-hub-helper@pi-status.timer
check "helper: its run succeeded" helper_result pi-status
check "helper: the page has its figure" page_matches pi-status caption '^(CPU temperature|No temperature sensor) [0-9]'
before=$(page_field pi-status caption)
sleep 1
box display-hub-helper run-now pi-status >/dev/null || bad "helper: run-now"
check "helper: run-now updates the page" test "$before" != "$(page_field pi-status caption)"
check "helper: list shows it" listed '^pi-status +5min +pi-status +worked'
check "helper: update replaces its program" box sh -c 'display-hub-helper update pi-status --run /usr/local/share/display-hub/helpers/pi-status >/dev/null && cmp -s /usr/local/share/display-hub/helpers/pi-status /usr/local/lib/display-hub/helpers/pi-status'
check "helper: logs shows its runs" logged pi-status 'Updated 1 page: pi-status'
check "helper: it reaches the hub on loopback" box grep -q '"host": "127.0.0.1"' "$pistate/connection.json"
token=$(box stat -c '%a %u' "$pistate/token")
folder=$(box stat -c '%a %u' "$pistate")
check "helper: the token is 0600" test "${token% *}" = 600
check "helper: its folder is 0700" test "${folder% *}" = 700
check "helper: the token is its dynamic user's" in_dynamic_range "${token#* }" "${folder#* }"

check "helper: add refuses a name in use" refused display-hub-helper add pi-status --run /bin/true

# The probe: on a one-minute timer, so the timer is seen running it again by itself.
box install -m 0755 /root/display-hub/probe-helper /root/probe-helper
out=$(box display-hub-helper add probe --run /root/probe-helper --every 1min 2>&1) || { echo "$out"; bad "helper: add probe"; }
probe_runs() {
  for _ in $(seq 1 100); do
    [ "$(box cat "$probestate/runs" 2>/dev/null || echo 0)" -ge "$1" ] && return 0
    sleep 1
  done
  return 1
}
check "helper: the timer runs it again (every 1min)" probe_runs 2
check "helper: list shows the interval" listed '^probe +1min +probe +worked'
probe=$(box cat "$probestate/probe.txt")
while read -r result what; do
  case $what in
    "send to its own feed") check "probe: can $what" test "$result" = OPEN ;;
    *) check "probe: can't $what" test "$result" = blocked ;;
  esac
done < <(grep -E '^(OPEN|blocked) ' <<<"$probe")
probe_uid=$(sed -n 's/^uid //p' <<<"$probe")
check "probe: its token is 0600 and its own" grep -qx "mode 600 $probe_uid" <<<"$probe"
check "probe: each helper has its own user" test "$probe_uid" != "${token#* }"
check "probe: the hub took its page" page_matches probe text '^probe 2'

check "helper: no invite in the journal" not_in_journal 'dh1\.'
check "helper: no helper token in the journal" not_in_journal 'dhc_'
check "helper: no invite left in its folder" not_in_folder 'dh1\.' "$pistate"
score=$(box systemd-analyze security display-hub-helper@pi-status.service --no-pager 2>/dev/null | tail -1)
echo "     systemd-analyze security (helper): $score"
exposure=$(grep -oE '[0-9]+\.[0-9]+' <<<"$score" | head -1)
check "helper: the exposure score is low (under $helper_max)" awk "BEGIN { exit !(${exposure:-9} < $helper_max) }"

# The upgrade, as the one-line install runs it: the release download, unpacked into a folder from
# mktemp -d and installed from there, then the folder deleted. Packed by the release's own script,
# here around the x86-64 programs.
tarball=$(bash "$root/tools/release/pi-tarball.sh" "$rel" 0.0.0-test "$work/pi")
docker cp "$tarball" "$name:/root/display-hub-pi.tar.gz"
# shellcheck disable=SC2016  # $d is expanded by the inner bash, on purpose
out=$(box bash -c 'd=$(mktemp -d) && echo "folder $d" && cd "$d" && tar -xzf /root/display-hub-pi.tar.gz && bash display-hub-pi/install.sh; s=$?; cd / && rm -rf "$d"; exit $s' 2>&1) || { echo "$out"; bad "install.sh again, from the download"; }
download_dir=$(sed -n 's/^folder //p' <<<"$out" | head -1)
check "upgrade: ran from a folder under /tmp" test "${download_dir#/tmp/}" != "$download_dir"
check "upgrade: and that folder is gone" gone "$download_dir"
check "upgrade: the kept copy of install.sh is still there (0755, root's)" is_mode "755 root:root" "$share/install.sh"
check "upgrade: the kept firewall rule is still there (0644, root's)" is_mode "644 root:root" "$share/nftables-display-hub.conf"
check "upgrade: the firewall still loads" firewall_loads
check "upgrade: backed up first" grep -q "Backed up the current hub" <<<"$out"
check "upgrade: settings kept" grep -q "Kept your" <<<"$out"
# shellcheck disable=SC2016  # $0 is expanded by the inner bash, on purpose
check "upgrade: no pairing string is printed" bash -c '! grep -qE "dh1\." <<<"$0"' "$out"
check "the pairing string was never logged" not_in_journal "$install_pairing"
check "upgrade: running, same key" bash -c "docker exec $name systemctl is-active --quiet display-hub && [ \"\$(docker exec $name runuser -u display-hub -- hubd fingerprint)\" = '$key' ]"
check "upgrade: helpers kept" listed '^pi-status +5min'
check "upgrade: a helper still runs" box sh -c 'display-hub-helper run-now pi-status >/dev/null'

# Removing a helper cuts it off at the hub and deletes its files. A copy of its folder (made by
# root, as only a test would) shows the old token is refused.
box cp -a "$pistate" /root/old-pi-status
box chown -R root:root /root/old-pi-status
old_token_refused() {
  local s
  s=$(box sh -c 'echo "{\"value\": 1}" | HUB_SEND_DIR=/root/old-pi-status HUB_SEND_STORE=file hub-send page pi-status --kind big_number --data - 2>&1' || true)
  grep -q "revoked" <<<"$s"
}
revoked_on_hub() { local o; o=$(as_hub hubd helper list); grep -qE "REVOKED .* Pi helper $1 " <<<"$o"; }
out=$(box display-hub-helper remove pi-status 2>&1) || { echo "$out"; bad "remove pi-status"; }
check "remove: says it cut it off" grep -q "Cut pi-status off at the hub" <<<"$out"
check "remove: the hub lists it as revoked" revoked_on_hub pi-status
check "remove: the hub refuses its old token" old_token_refused
check "remove: its timer is off" refused systemctl is-enabled --quiet display-hub-helper@pi-status.timer
check "remove: its program is gone" gone /usr/local/lib/display-hub/helpers/pi-status
check "remove: its folder and token are gone" gone "$pistate"
check "remove: its settings are gone" gone /etc/systemd/system/display-hub-helper@pi-status.timer.d
check "remove: list leaves it out" refused sh -c 'display-hub-helper list | grep "^pi-status "'
check "remove: the other helper is untouched" listed '^probe +1min'
check "uninstall: refused while a helper is set up" refused bash "$share/install.sh" --uninstall
box display-hub-helper remove probe >/dev/null

# Remove on a helper the hub has already cut off is still a success, with no warning.
box display-hub-helper add twice --run /usr/local/share/display-hub/helpers/pi-status >/dev/null 2>&1 || bad "helper: add twice"
twice_id=$(box sed -n 's/^ *"helper_id": *"\([A-Za-z0-9_-]*\)".*/\1/p' /var/lib/private/display-hub-helper-twice/connection.json | head -1)
check "remove: the hub revokes it first" as_hub hubd helper revoke "$twice_id"
status=0
out=$(box display-hub-helper remove twice 2>&1) || status=$?
check "remove: an already revoked helper is removed with exit 0" test "$status" = 0
clean_removal() { ! grep -q "display-hub-helper:" <<<"$out" && grep -q "Removed twice" <<<"$out"; }
check "remove: and says so without a warning" clean_removal
check "remove: twice is gone" gone /var/lib/private/display-hub-helper-twice

# A login that fails after the hub paired it (stood in for by a wrapper that fails once the real
# login is done) leaves nothing active at the hub. A look-alike helper paired elsewhere under the
# same label beforehand must stay active: only the pairing the failed add made is revoked.
invite=$(as_hub hubd pair-helper --local --label "Pi helper orphan" --feed orphan | grep -oE 'dh1\.[A-Za-z0-9_-]+\.[0-9a-f]{8}' | head -1)
printf '%s\n' "$invite" | docker exec -i "$name" env HUB_SEND_DIR=/root/lookalike HUB_SEND_STORE=file /usr/local/bin/hub-send login >/dev/null
unset invite
lookalike_id=$(box sed -n 's/^ *"helper_id": *"\([A-Za-z0-9_-]*\)".*/\1/p' /root/lookalike/connection.json | head -1)
check "orphan: a look-alike helper is paired first" test -n "$lookalike_id"
box mv /usr/local/bin/hub-send /usr/local/bin/hub-send.real
box sh -c 'printf "#!/bin/sh\n/usr/local/bin/hub-send.real \"\$@\"\nexit 1\n" > /usr/local/bin/hub-send && chmod 0755 /usr/local/bin/hub-send'
status=0
out=$(box display-hub-helper add orphan --run /usr/local/share/display-hub/helpers/pi-status 2>&1) || status=$?
box mv -f /usr/local/bin/hub-send.real /usr/local/bin/hub-send
check "orphan: add fails when the login does" test "$status" != 0
check "orphan: and sets nothing up" gone /var/lib/private/display-hub-helper-orphan
# The plain list is tab-separated: id, active or revoked, label.
orphan_state() { as_hub hubd helper list --plain | awk -F'\t' -v s="$1" -v skip="$lookalike_id" '$1 != skip && $2 == s && $3 == "Pi helper orphan" { f = 1 } END { exit !f }'; }
not_active() { ! orphan_state active; }
check "orphan: the hub holds no active credential for it" not_active
check "orphan: the hub lists it as revoked" orphan_state revoked
lookalike_active() { as_hub hubd helper list --plain | awk -F'\t' -v id="$lookalike_id" '$1 == id && $2 == "active" { f = 1 } END { exit !f }'; }
check "orphan: the look-alike helper is still active" lookalike_active
as_hub hubd helper revoke "$lookalike_id" >/dev/null
box rm -rf /root/lookalike

# From the kept copy, as docs/deploy-raspberry-pi.md says: it removes itself as it finishes.
out=$(box bash "$share/install.sh" --uninstall 2>&1) || { echo "$out"; bad "uninstall"; }
check "uninstall: says it finished" grep -q "Removed the service" <<<"$out"
check "uninstall: the service is gone" bash -c "! docker exec $name systemctl cat display-hub >/dev/null 2>&1"
check "uninstall: the data is kept" box test -f /var/lib/display-hub/identity/hub-key.pem
check "uninstall: the helper tools are gone" gone /usr/local/sbin/display-hub-helper
check "uninstall: hub-send is gone" gone /usr/local/bin/hub-send
check "uninstall: its kept copy is gone" gone "$share/install.sh"
check "uninstall: the firewall rule is kept while it is included" box test -f "$share/nftables-display-hub.conf"
check "uninstall: and says so" grep -q "because your firewall settings include it" <<<"$out"
check "uninstall: the firewall still loads" firewall_loads
# An include in a file under /etc/nftables.d/ counts too, and the message names that file.
box sed -i "\|$share/nftables-display-hub.conf|d" /etc/nftables.conf
box sh -c "mkdir -p /etc/nftables.d && echo 'include \"$share/nftables-display-hub.conf\"' > /etc/nftables.d/hub.conf"
out=$(box bash /root/display-hub/install.sh --uninstall 2>&1) || { echo "$out"; bad "uninstall with an include in nftables.d"; }
check "uninstall: an include under nftables.d keeps the rule" box test -f "$share/nftables-display-hub.conf"
check "uninstall: and names that file, not nftables.conf" grep -q "/etc/nftables.d/hub.conf" <<<"$out"
box rm -f /etc/nftables.d/hub.conf
# A commented-out include is not an include.
box sh -c "echo '# include \"$share/nftables-display-hub.conf\"' >> /etc/nftables.conf"
box bash /root/display-hub/install.sh --uninstall >/dev/null
check "uninstall: once not included (a comment doesn't count), the firewall rule and its folder go too" gone "$share"

if [ "$failures" = 0 ]; then
  echo "Pi install test (container, not a Pi): all passed"
else
  echo "Pi install test (container, not a Pi): $failures failed"
  exit 1
fi
