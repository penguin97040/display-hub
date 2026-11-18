#!/usr/bin/env bash
# Packs and signs a firmware build as a development package (.dhfw), with everything a first USB
# install needs, then checks it with `hub-firmware inspect`.
#
#   firmware/tools/package-dev.sh <build folder> <key file> <output .dhfw>
#
# The offsets come from the build's own flasher_args.json and the version from the built app image,
# so nothing here needs changing when the layout or the version does. The key is also the package's
# whole key set, so a display installed from it trusts that key only.
#
# The key's passphrase is asked for on the terminal, or read as one line from standard input. To
# read it from a file instead, set DH_PASSPHRASE_FILE to that file's path.
#
# Set HUB_FIRMWARE to the hub-firmware program to use (default: `hub-firmware` on the PATH).
# Needs python3.
set -euo pipefail

DEVICE_TYPE=display-hub-waveshare-amoled-1.8
HARDWARE=waveshare-amoled-1.8-v2
CHIP=esp32s3
FLASH_SIZE=16MB
MIN_FLASH=0x1000000
SECURITY_COUNTER=0
SUMMARY="Development build. Not for release."

die() {
  echo "package-dev.sh: $*" >&2
  exit 1
}

[ $# -eq 3 ] || die "usage: package-dev.sh <build folder> <key file> <output .dhfw>"
build=$1
key=$2
out=$3
tool=${HUB_FIRMWARE:-hub-firmware}

[ -f "$build/flasher_args.json" ] || die "no flasher_args.json in $build: is it an ESP-IDF build folder?"
[ -f "$key" ] || die "no key file at $key"
command -v "$tool" > /dev/null || die "can't find $tool (set HUB_FIRMWARE to its path)"
if [ -n "${DH_PASSPHRASE_FILE:-}" ] && [ ! -f "$DH_PASSPHRASE_FILE" ]; then
  die "DH_PASSPHRASE_FILE names a file that doesn't exist"
fi

# One value per line: the app's offset, file and version, then each USB part as name, offset and
# file, in flash order. Paths are made absolute from the build folder.
layout=$(python3 -I - "$build" "$CHIP" "$FLASH_SIZE" << 'EOF'
import json, os, struct, sys

build, chip, flash_size = sys.argv[1:4]
with open(os.path.join(build, "flasher_args.json"), encoding="utf-8") as f:
    args = json.load(f)

def fail(msg):
    sys.exit("package-dev.sh: " + msg)

got_chip = args.get("extra_esptool_args", {}).get("chip")
if got_chip != chip:
    fail(f"the build is for {got_chip!r}, not {chip}")
got_size = args.get("flash_settings", {}).get("flash_size")
if got_size != flash_size:
    fail(f"the build is set for {got_size!r} of flash, not {flash_size}")

def entry(name):
    e = args.get(name)
    if not e or "offset" not in e or "file" not in e:
        fail(f"flasher_args.json has no {name!r} entry")
    if e.get("encrypted", "false") != "false":
        fail(f"{name} is marked encrypted; flash encryption is meant to be off")
    path = os.path.join(build, e["file"])
    if not os.path.isfile(path):
        fail(f"{name} file {path} is missing")
    return int(e["offset"], 16), os.path.abspath(path)

app_offset, app_file = entry("app")

# The version, from the image's esp_app_desc_t: a 24-byte image header and an 8-byte segment
# header, then the description, whose 32-byte version field starts 16 bytes in.
with open(app_file, "rb") as f:
    head = f.read(32 + 48)
if len(head) < 80 or head[0] != 0xE9:
    fail(f"{app_file} isn't an ESP-IDF app image")
(magic,) = struct.unpack_from("<I", head, 32)
if magic != 0xABCD5432:
    fail(f"{app_file} has no app description")
try:
    version = head[48:80].split(b"\0", 1)[0].decode("ascii")
except UnicodeDecodeError:
    version = ""
if not version or not version.isprintable():
    fail("the app's version is empty or unreadable")

parts = []
for name, label in (("bootloader", "bootloader"), ("partition-table", "partition-table"),
                    ("otadata", "otadata")):
    off, path = entry(name)
    parts.append((off, label, path))
parts.sort()

print(f"0x{app_offset:x}")
print(app_file)
print(version)
for off, label, path in parts:
    print(label)
    print(f"0x{off:x}")
    print(path)
EOF
)
mapfile -t lines <<< "$layout"
[ "${#lines[@]}" -eq 12 ] || die "couldn't read the build's layout"

app_offset=${lines[0]}
app_file=${lines[1]}
version=${lines[2]}
usb_parts=()
for i in 3 6 9; do
  usb_parts+=(--usb-part "${lines[i]}@${lines[i + 1]}=${lines[i + 2]}")
done

echo "Packing $app_file as $DEVICE_TYPE $version for $HARDWARE, app at $app_offset" >&2

sign=("$tool" sign --key "$key" --image "$app_file"
  --device-type "$DEVICE_TYPE" --hardware "$HARDWARE"
  --version "$version" --security-counter "$SECURITY_COUNTER" --summary "$SUMMARY"
  --key-set "$key" "${usb_parts[@]}"
  --app-offset "$app_offset" --chip "$CHIP" --min-flash "$MIN_FLASH"
  --out "$out")
if [ -n "${DH_PASSPHRASE_FILE:-}" ]; then
  "${sign[@]}" < "$DH_PASSPHRASE_FILE"
else
  "${sign[@]}"
fi

"$tool" inspect "$out"
