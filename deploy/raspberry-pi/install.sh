#!/usr/bin/env bash
# Installs or upgrades Display Hub on a Raspberry Pi (Raspberry Pi OS Bookworm or Trixie, 64-bit).
#
#   sudo bash install.sh              install, or upgrade in place
#   sudo bash /usr/local/share/display-hub/install.sh --uninstall
#                                     stop and remove the service, binaries and helper tools; keep
#                                     data and settings
#
# Run it from a folder holding `hubd` and `hub-send` (the aarch64 builds) and the files next to this
# script. Safe to run again: it never overwrites /etc/display-hub/hubd.toml, never deletes data, and
# backs the hub up before replacing a running version. It installs what helpers need, but turns no
# helper on: that's `sudo display-hub-helper add`. That folder may be thrown away afterwards (the
# one-line install unpacks it under /tmp): this script and the firewall rule are kept in
# /usr/local/share/display-hub/, for removing it and for the firewall. See docs/deploy-raspberry-pi.md.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
user=display-hub
bin=/usr/local/bin/hubd
unit=/etc/systemd/system/display-hub.service
conf_dir=/etc/display-hub
data_dir=/var/lib/display-hub
backups=/var/backups/display-hub
send_bin=/usr/local/bin/hub-send
helper_cmd=/usr/local/sbin/display-hub-helper
helper_units=(/etc/systemd/system/display-hub-helper@.service /etc/systemd/system/display-hub-helper@.timer)
helper_programs=/usr/local/lib/display-hub/helpers
share=/usr/local/share/display-hub
examples=$share/helpers
# Kept after the folder this ran from has gone: the uninstaller, and the firewall rule, which
# /etc/nftables.conf may include, so it must stay put.
kept_script=$share/install.sh
firewall=$share/nftables-display-hub.conf

say() { printf '%s\n' "$*"; }
fail() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
as_hub() { runuser -u "$user" -- "$@"; }

[ "$(id -u)" = 0 ] || fail "run it with sudo"
command -v systemctl >/dev/null || fail "this needs systemd"
command -v runuser >/dev/null || fail "this needs runuser (util-linux)"

# Installs a file atomically, so a running helper never sees half of it.
put() { install -m "$1" -o root -g root "$2" "$3.new" && mv -f "$3.new" "$3"; }

if [ "${1:-}" = "--uninstall" ]; then
  # Helpers hold tokens for this hub: removing them properly also cuts them off there.
  set_up=$(find "$helper_programs" -mindepth 1 -maxdepth 1 -type f -printf '%f ' 2>/dev/null || true)
  if [ -n "$set_up" ]; then
    fail "these helpers are still set up: $set_up
Remove each one first (it also cuts it off at the hub): sudo display-hub-helper remove <name>"
  fi
  systemctl disable --now display-hub 2>/dev/null || true
  rm -f "$unit" "$bin" "$send_bin" "$helper_cmd" "${helper_units[@]}"
  rm -rf "$examples"
  # A firewall that includes the rule would fail to load at the next boot without it, so it stays
  # until that line is gone.
  kept_firewall=0
  if grep -qsF "$firewall" /etc/nftables.conf || grep -rqsF "$firewall" /etc/nftables.d; then
    kept_firewall=1
  else
    rm -f "$firewall"
  fi
  # Bash has read this whole if-block already, so this copy of the script can go even while it is
  # the one running.
  rm -f "$kept_script"
  rmdir "$helper_programs" /usr/local/lib/display-hub "$share" 2>/dev/null || true
  systemctl daemon-reload
  say "Removed the service, $bin, hub-send and the helper tools."
  say "Kept $data_dir (the hub's identity and data), $conf_dir and $backups."
  say "Delete them yourself only if you'll never use this hub again: every display and PC trusts its key."
  if [ "$kept_firewall" = 1 ]; then
    say ""
    say "Kept $firewall, because your firewall settings include it. To remove it, take the"
    say "line that names it out of /etc/nftables.conf, then: sudo rm -r $share"
  fi
  exit 0
fi
[ $# -eq 0 ] || fail "unknown argument: $1"

for f in hubd hub-send install.sh nftables-display-hub.conf display-hub.service hubd.toml \
  display-hub-helper@.service display-hub-helper@.timer display-hub-helper helpers/pi-status; do
  [ -f "$here/$f" ] || fail "$f is missing from $here"
done
# The binaries are staged beside where they go and tried there first, so a copy that lost its
# execute bits on the way (from Windows, or a CI download) still works, and a wrong build is
# refused before anything changes.
trap 'rm -f "$bin.new" "$send_bin.new"' EXIT
install -m 0755 "$here/hubd" "$bin.new"
install -m 0755 "$here/hub-send" "$send_bin.new"
for b in "$bin" "$send_bin"; do
  "$b.new" --version >/dev/null 2>&1 || fail "$here/${b##*/} doesn't run on this machine (is it the aarch64 build?)"
done
new_version=$("$bin.new" --version)

# The service user: no login, no home of its own beyond the data folder.
if ! id "$user" >/dev/null 2>&1; then
  useradd --system --user-group --home-dir "$data_dir" --no-create-home \
    --shell /usr/sbin/nologin --comment "Display Hub" "$user"
  say "Created the $user system user."
fi

# A first install has no database yet. Only then does the end of this script make a pairing string.
fresh_install=1
[ -f "$data_dir/hub.db" ] && fresh_install=0

# Upgrading a hub that has data: back it up first, while it still runs the old version.
if [ -f "$data_dir/hub.db" ] && [ -x "$bin" ]; then
  install -d -m 0700 -o "$user" -g "$user" "$backups"
  stamp=$(date -u +%Y-%m-%dT%H-%M-%SZ)
  as_hub "$bin" backup "$backups/before-upgrade-$stamp" >/dev/null
  say "Backed up the current hub to $backups/before-upgrade-$stamp."
fi

mv -f "$bin.new" "$bin"
install -d -m 0755 "$conf_dir"
if [ ! -f "$conf_dir/hubd.toml" ]; then
  install -m 0644 "$here/hubd.toml" "$conf_dir/hubd.toml"
  say "Installed $conf_dir/hubd.toml (all defaults)."
else
  say "Kept your $conf_dir/hubd.toml."
fi
install -m 0644 "$here/display-hub.service" "$unit"

# Helpers: hub-send, the helper units and the command that sets helpers up, and the example. Helpers
# already set up keep running, with the new units from the next run.
mv -f "$send_bin.new" "$send_bin"
put 0755 "$here/display-hub-helper" "$helper_cmd"
put 0644 "$here/display-hub-helper@.service" "${helper_units[0]}"
put 0644 "$here/display-hub-helper@.timer" "${helper_units[1]}"
install -d -m 0755 -o root -g root "$helper_programs" "$share" "$examples"
put 0755 "$here/helpers/pi-status" "$examples/pi-status"
# The uninstaller and the firewall rule, kept where they last. Put atomically: replacing this
# script in place could change what a running copy of it reads next.
put 0755 "$here/install.sh" "$kept_script"
put 0644 "$here/nftables-display-hub.conf" "$firewall"

systemctl daemon-reload
systemctl enable display-hub >/dev/null 2>&1
systemctl restart display-hub

for _ in $(seq 1 30); do
  systemctl is-active --quiet display-hub && [ -f "$data_dir/identity/hub-cert.pem" ] && break
  sleep 1
done
if ! systemctl is-active --quiet display-hub; then
  journalctl -u display-hub -n 20 --no-pager >&2 || true
  fail "the service didn't start; the log is above"
fi

say ""
say "Display Hub is running: $new_version"
as_hub "$bin" check || say "(Problems above are worth a look. An unsynchronised clock fixes itself once the Pi reaches a time server.)"
say ""
if [ "$fresh_install" = 1 ]; then
  # Printed to this terminal only, never saved: it works once, for 10 minutes.
  say "Next, connect the Display Hub app on your PC. Copy the whole line below that starts with"
  say "dh1 and paste it into the app. It works once, within 10 minutes. Don't share it or save it."
  say ""
  as_hub "$bin" pair-admin || say "(Couldn't make one. Try again with: sudo -u $user hubd pair-admin)"
else
  say "To connect another PC to this hub, make a pairing string:"
  say "  sudo -u $user hubd pair-admin"
fi
say ""
say "To send your own data to the hub from this Pi, see: sudo display-hub-helper help"
