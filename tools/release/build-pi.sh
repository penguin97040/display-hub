#!/usr/bin/env bash
# Builds hubd and hub-send for the Raspberry Pi: aarch64 against glibc 2.36, so one build runs on
# Raspberry Pi OS Bookworm and Trixie. Then checks each is an ARM64 program that needs no newer
# glibc, and prints the folder holding them, for tools/release/pi-tarball.sh. CI and the release
# workflow both run it, after .github/actions/zigbuild.
#
#   bash tools/release/build-pi.sh
#
# Needs zig, cargo-zigbuild and readelf.
set -euo pipefail

[ $# -eq 0 ] || { echo "usage: $0" >&2; exit 2; }
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
target=aarch64-unknown-linux-gnu
cargo zigbuild --release --locked -p hubd -p hub-send --target "$target.2.36" >&2
bins=${CARGO_TARGET_DIR:-$root/target}/$target/release
for b in hubd hub-send; do
  bin=$bins/$b
  # Captured first: with pipefail, grep -q stopping early could fail the pipe.
  header=$(readelf -h "$bin")
  grep -q AArch64 <<<"$header" || { echo "$bin isn't an ARM64 program" >&2; exit 1; }
  newest=$(readelf -V "$bin" | grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -1)
  echo "$b needs at most $newest" >&2
  [ "$(printf '%s\nGLIBC_2.36\n' "$newest" | sort -V | tail -1)" = GLIBC_2.36 ] ||
    { echo "$b needs $newest, newer than Bookworm's GLIBC_2.36" >&2; exit 1; }
done
echo "$bins"
