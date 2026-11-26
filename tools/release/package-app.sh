#!/usr/bin/env bash
# Packs the desktop app and the command-line tools for one platform, after
# `cargo build --release --locked` of hub-manager, hub-send, hub-firmware, display-simulator and
# usage-helper. The release workflow runs it on each runner; it prints the file's path.
#
#   bash tools/release/package-app.sh windows <version> <bin-dir> <out-dir> [<firmware>]   display-hub-windows-x64-<version>.zip
#   bash tools/release/package-app.sh macos   <version> <bin-dir> <out-dir> [<firmware>]   display-hub-macos-arm64-<version>.zip
#   bash tools/release/package-app.sh linux   <version> <bin-dir> <out-dir> [<firmware>]   display-hub-linux-x64-<version>.tar.gz
#
# Each holds one folder of the same name with the programs, LICENSE and THIRD-PARTY-NOTICES.txt. On
# macOS the app is "Display Hub.app", signed ad hoc (no paid certificate, but Apple silicon runs
# nothing unsigned), with its own copy of THIRD-PARTY-NOTICES.txt in Contents/Resources/.
#
# <firmware> is the official firmware package (.dhfw), already checked by
# check-official-firmware.py. It goes in under its own name where the app looks for it: firmware/
# beside the app, or Display Hub.app/Contents/Resources/firmware/ on macOS. A release always gives
# one; without it the download has no firmware in it.
# The zips need Python 3 (on the Windows and macOS runners); macOS also uses codesign and plutil,
# which come with it.
#
# The same programs give the same file: entries in name order, programs 0755 and everything else
# 0644, owned by root in the tarball, and every time set to $SOURCE_DATE_EPOCH, or else the last
# commit's.
set -euo pipefail

[ $# -eq 4 ] || [ $# -eq 5 ] || {
  echo "usage: $0 <windows|macos|linux> <version> <bin-dir> <out-dir> [<firmware package>]" >&2
  exit 2
}
platform=$1
version=$2
bins=$3
out=$4
firmware=${5:-}
root=$(cd "$(dirname "$0")/../.." && pwd)
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || { echo "not a version: $version" >&2; exit 2; }
tools=(hub-send hub-firmware display-simulator usage-helper usage-helper-sync)

case $platform in
  windows) name=display-hub-windows-x64-$version; exe=.exe ;;
  macos) name=display-hub-macos-arm64-$version; exe= ;;
  linux) name=display-hub-linux-x64-$version; exe= ;;
  *) echo "unknown platform: $platform" >&2; exit 2 ;;
esac
for b in hub-manager "${tools[@]}"; do
  [ -f "$bins/$b$exe" ] || { echo "no $bins/$b$exe" >&2; exit 2; }
done
if [ $# -eq 5 ]; then
  [ -f "$firmware" ] || { echo "no firmware package at ${firmware:-(nothing given)}" >&2; exit 2; }
  [[ ${firmware##*/} =~ ^[a-z0-9][a-z0-9.-]*\.dhfw$ ]] || { echo "not a package's name: ${firmware##*/}" >&2; exit 2; }
fi

stamp=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct 2>/dev/null || date +%s)}
[[ $stamp =~ ^[0-9]+$ ]] || { echo "not a time in seconds: $stamp" >&2; exit 2; }
# The first that really runs Python 3: on Windows, "python3" may be only a pointer to the Store.
py=
for c in python3 python; do
  if "$c" -c 'import sys; sys.exit(sys.version_info[0] != 3)' >/dev/null 2>&1; then py=$c; break; fi
done

# Zips the folder $name in $stage into $1, with these paths in it (relative to $stage) as programs.
# Python here may be a Windows program, which doesn't read Git Bash's /d/... paths: hence relative
# paths, run from $stage.
pack_zip() {
  local dest=$1
  shift
  [ -n "$py" ] || { echo "packing a zip needs Python 3" >&2; exit 2; }
  (cd "$stage" && STAMP=$stamp "$py" -I - "$name" "$name.zip" "$@" <<'PY'
import os, sys, time, zipfile
top, dest, programs = sys.argv[1], sys.argv[2], set(sys.argv[3:])
# A zip keeps local time without a zone, from 1980 on: the stamp is written as UTC.
when = time.gmtime(max(int(os.environ["STAMP"]), 315532800))[:6]
entries = []
for folder, dirs, files in os.walk(top):
    rel = folder.replace(os.sep, "/")
    entries.append(rel + "/")
    entries.extend(rel + "/" + f for f in files)
missing = programs - set(entries)
if missing:
    sys.exit(f"not in the folder: {sorted(missing)}")
with zipfile.ZipFile(dest, "w") as z:
    for path in sorted(entries):
        info = zipfile.ZipInfo(path, when)
        info.create_system = 3  # Unix, so the modes below are used, whichever system packs it
        if path.endswith("/"):
            info.external_attr = (0o040755 << 16) | 0x10
            z.writestr(info, b"")
            continue
        mode = 0o755 if path in programs else 0o644
        info.external_attr = (0o100000 | mode) << 16
        info.compress_type = zipfile.ZIP_DEFLATED
        with open(path, "rb") as f:
            z.writestr(info, f.read(), compresslevel=9)
PY
  )
  mv "$stage/$name.zip" "$dest"
}

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir=$stage/$name
mkdir -m 0755 "$dir"
cp "$root/LICENSE" "$dir/LICENSE"
cp "$root/THIRD-PARTY-NOTICES.txt" "$dir/THIRD-PARTY-NOTICES.txt"
for b in "${tools[@]}"; do
  cp "$bins/$b$exe" "$dir/$b$exe"
  chmod 0755 "$dir/$b$exe"
done
mkdir -p "$out"
out=$(cd "$out" && pwd)

# Puts the official firmware package in the folder $1, where the app looks for it.
add_firmware() {
  [ -n "$firmware" ] || return 0
  mkdir -m 0755 "$1/firmware"
  cp "$firmware" "$1/firmware/${firmware##*/}"
  chmod 0644 "$1/firmware/${firmware##*/}"
}

case $platform in
  windows)
    cp "$bins/hub-manager.exe" "$dir/hub-manager.exe"
    add_firmware "$dir"
    programs=("$name/hub-manager.exe")
    for b in "${tools[@]}"; do programs+=("$name/$b.exe"); done
    pack_zip "$out/$name.zip" "${programs[@]}"
    echo "$out/$name.zip"
    ;;
  macos)
    app="$dir/Display Hub.app"
    mkdir -p "$app/Contents/MacOS"
    cp "$bins/hub-manager" "$app/Contents/MacOS/hub-manager"
    chmod 0755 "$app/Contents/MacOS/hub-manager"
    # Before signing, so the app's signature covers them: the notices travel inside the app too,
    # for when it is copied to Applications without the rest of the folder.
    mkdir -m 0755 "$app/Contents/Resources"
    cp "$root/THIRD-PARTY-NOTICES.txt" "$app/Contents/Resources/THIRD-PARTY-NOTICES.txt"
    add_firmware "$app/Contents/Resources"
    # Apple's version fields take numbers only, so a release candidate's suffix is left off.
    short=${version%%-*}
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>Display Hub</string>
  <key>CFBundleExecutable</key>
  <string>hub-manager</string>
  <key>CFBundleIdentifier</key>
  <string>io.github.penguin97040.display-hub</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>Display Hub</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$short</string>
  <key>CFBundleVersion</key>
  <string>$short</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.utilities</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST
    plutil -lint "$app/Contents/Info.plist"
    for b in "${tools[@]}"; do
      codesign --force -s - "$dir/$b"
    done
    codesign --force --deep -s - "$app"
    codesign --verify --deep --strict "$app"
    # Python rather than ditto, which keeps each file's own time, owner and extended attributes in
    # the zip, in the order the disk lists them. A signature covers the files' contents, not their
    # times or modes, so it survives this; the check below is on the unpacked zip.
    programs=("$name/Display Hub.app/Contents/MacOS/hub-manager")
    for b in "${tools[@]}"; do programs+=("$name/$b"); done
    pack_zip "$out/$name.zip" "${programs[@]}"
    mkdir "$stage/check"
    ditto -x -k "$out/$name.zip" "$stage/check"
    codesign --verify --deep --strict "$stage/check/$name/Display Hub.app"
    for b in "${tools[@]}"; do codesign --verify --strict "$stage/check/$name/$b"; done
    echo "$out/$name.zip"
    ;;
  linux)
    cp "$bins/hub-manager" "$dir/hub-manager"
    chmod 0755 "$dir/hub-manager"
    chmod 0644 "$dir/LICENSE" "$dir/THIRD-PARTY-NOTICES.txt"
    add_firmware "$dir"
    tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$stamp" --format=gnu \
      -C "$stage" -cf - "$name" | gzip -9 -n > "$out/$name.tar.gz"
    echo "$out/$name.tar.gz"
    ;;
esac
