#!/usr/bin/env bash
# Packs the desktop app and the command-line tools for one platform, after
# `cargo build --release --locked` of hub-manager, hub-send, hub-firmware, display-simulator and
# usage-helper. The release workflow runs it on each runner; it prints the file's path.
#
#   bash tools/release/package-app.sh windows <version> <bin-dir> <out-dir>   display-hub-windows-x64-<version>.zip
#   bash tools/release/package-app.sh macos   <version> <bin-dir> <out-dir>   display-hub-macos-arm64-<version>.zip
#   bash tools/release/package-app.sh linux   <version> <bin-dir> <out-dir>   display-hub-linux-x64-<version>.tar.gz
#
# Each holds one folder of the same name with the programs and LICENSE. On macOS the app is
# "Display Hub.app", signed ad hoc (no paid certificate, but Apple silicon runs nothing unsigned).
# Windows needs Python for the zip; macOS uses ditto and codesign, which come with it.
set -euo pipefail

[ $# -eq 4 ] || { echo "usage: $0 <windows|macos|linux> <version> <bin-dir> <out-dir>" >&2; exit 2; }
platform=$1
version=$2
bins=$3
out=$4
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

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
dir=$stage/$name
mkdir "$dir"
cp "$root/LICENSE" "$dir/LICENSE"
for b in "${tools[@]}"; do
  cp "$bins/$b$exe" "$dir/$b$exe"
  chmod 0755 "$dir/$b$exe"
done
mkdir -p "$out"
out=$(cd "$out" && pwd)

case $platform in
  windows)
    cp "$bins/hub-manager.exe" "$dir/hub-manager.exe"
    # Relative paths: Python here is a Windows program, which doesn't read Git Bash's /d/... paths.
    (cd "$stage" && python - "$name" "$name.zip" <<'PY'
import os, sys, zipfile
top, dest = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    for folder, dirs, files in os.walk(top):
        dirs.sort()
        for f in sorted(files):
            path = os.path.join(folder, f)
            z.write(path, path.replace(os.sep, "/"))
PY
    )
    mv "$stage/$name.zip" "$out/$name.zip"
    echo "$out/$name.zip"
    ;;
  macos)
    app="$dir/Display Hub.app"
    mkdir -p "$app/Contents/MacOS"
    cp "$bins/hub-manager" "$app/Contents/MacOS/hub-manager"
    chmod 0755 "$app/Contents/MacOS/hub-manager"
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
  <string>org.displayhub.manager</string>
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
    ditto -c -k --keepParent "$dir" "$out/$name.zip"
    echo "$out/$name.zip"
    ;;
  linux)
    cp "$bins/hub-manager" "$dir/hub-manager"
    chmod 0755 "$dir/hub-manager"
    chmod 0644 "$dir/LICENSE"
    stamp=$(git -C "$root" log -1 --format=%ct 2>/dev/null || date +%s)
    tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$stamp" --format=gnu \
      -C "$stage" -cf - "$name" | gzip -9 -n > "$out/$name.tar.gz"
    echo "$out/$name.tar.gz"
    ;;
esac
