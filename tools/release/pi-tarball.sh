#!/usr/bin/env bash
# Packs the Raspberry Pi download: a display-hub-pi folder holding hubd, hub-send, everything in
# deploy/raspberry-pi/, LICENSE and THIRD-PARTY-NOTICES.txt, as
# display-hub-pi-arm64-<version>.tar.gz. The release workflow runs it after building the two
# programs; it prints the file's path.
#
#   bash tools/release/pi-tarball.sh <folder with hubd and hub-send> <version> <output folder>
#
# Programs and scripts are 0755, everything else 0644, owned by root, with every time set to
# $SOURCE_DATE_EPOCH, or else the last commit's, so the same build gives the same file.
set -euo pipefail

[ $# -eq 3 ] || { echo "usage: $0 <bin-dir> <version> <out-dir>" >&2; exit 2; }
bins=$1
version=$2
out=$3
root=$(cd "$(dirname "$0")/../.." && pwd)
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || { echo "not a version: $version" >&2; exit 2; }
for b in hubd hub-send; do
  [ -f "$bins/$b" ] || { echo "no $bins/$b" >&2; exit 2; }
done

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir=$stage/display-hub-pi
mkdir "$dir"
cp -R "$root/deploy/raspberry-pi/." "$dir/"
cp "$bins/hubd" "$bins/hub-send" "$dir/"
cp "$root/LICENSE" "$root/THIRD-PARTY-NOTICES.txt" "$dir/"
find "$dir" -type d -exec chmod 0755 {} +
find "$dir" -type f -exec chmod 0644 {} +
chmod 0755 "$dir/hubd" "$dir/hub-send" "$dir/install.sh" "$dir/display-hub-helper" "$dir/helpers/pi-status"

stamp=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct 2>/dev/null || date +%s)}
[[ $stamp =~ ^[0-9]+$ ]] || { echo "not a time in seconds: $stamp" >&2; exit 2; }
mkdir -p "$out"
file=$(cd "$out" && pwd)/display-hub-pi-arm64-$version.tar.gz
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$stamp" --format=gnu \
  -C "$stage" -cf - display-hub-pi | gzip -9 -n > "$file"
echo "$file"
