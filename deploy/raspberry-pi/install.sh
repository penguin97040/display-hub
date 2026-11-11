#!/usr/bin/env bash
# Installs or upgrades Display Hub on a Raspberry Pi (Raspberry Pi OS Bookworm or Trixie, 64-bit).
#
#   sudo bash install.sh              install, or upgrade in place
#   sudo bash install.sh --uninstall  stop and remove the service and binary; keep data and settings
#
# Run it from a folder holding `hubd` (the aarch64 build) and the files next to this script.
# Safe to run again: it never overwrites /etc/display-hub/hubd.toml, never deletes data, and backs
# the hub up before replacing a running version. See docs/deploy-raspberry-pi.md.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
user=display-hub
bin=/usr/local/bin/hubd
unit=/etc/systemd/system/display-hub.service
conf_dir=/etc/display-hub
data_dir=/var/lib/display-hub
backups=/var/backups/display-hub

say() { printf '%s\n' "$*"; }
fail() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
as_hub() { runuser -u "$user" -- "$@"; }

[ "$(id -u)" = 0 ] || fail "run it with sudo"
command -v systemctl >/dev/null || fail "this needs systemd"
command -v runuser >/dev/null || fail "this needs runuser (util-linux)"

if [ "${1:-}" = "--uninstall" ]; then
  systemctl disable --now display-hub 2>/dev/null || true
  rm -f "$unit" "$bin"
  systemctl daemon-reload
  say "Removed the service and $bin."
  say "Kept $data_dir (the hub's identity and data), $conf_dir and $backups."
  say "Delete them yourself only if you'll never use this hub again: every display and PC trusts its key."
  exit 0
fi
[ $# -eq 0 ] || fail "unknown argument: $1"

for f in hubd display-hub.service hubd.toml; do
  [ -f "$here/$f" ] || fail "$f is missing from $here"
done
new_version=$("$here/hubd" --version) || fail "$here/hubd doesn't run on this machine (is it the aarch64 build?)"

# The service user: no login, no home of its own beyond the data folder.
if ! id "$user" >/dev/null 2>&1; then
  useradd --system --user-group --home-dir "$data_dir" --no-create-home \
    --shell /usr/sbin/nologin --comment "Display Hub" "$user"
  say "Created the $user system user."
fi

# Upgrading a hub that has data: back it up first, while it still runs the old version.
if [ -f "$data_dir/hub.db" ] && [ -x "$bin" ]; then
  install -d -m 0700 -o "$user" -g "$user" "$backups"
  stamp=$(date -u +%Y-%m-%dT%H-%M-%SZ)
  as_hub "$bin" backup "$backups/before-upgrade-$stamp" >/dev/null
  say "Backed up the current hub to $backups/before-upgrade-$stamp."
fi

install -m 0755 "$here/hubd" "$bin.new"
mv -f "$bin.new" "$bin"
install -d -m 0755 "$conf_dir"
if [ ! -f "$conf_dir/hubd.toml" ]; then
  install -m 0644 "$here/hubd.toml" "$conf_dir/hubd.toml"
  say "Installed $conf_dir/hubd.toml (all defaults)."
else
  say "Kept your $conf_dir/hubd.toml."
fi
install -m 0644 "$here/display-hub.service" "$unit"

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
say "Next, pair the desktop manager:"
say "  sudo -u $user hubd pair-admin"
