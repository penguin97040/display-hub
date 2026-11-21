#!/bin/sh
# Builds ESP-IDF's NVS for a computer over the firmware's partition table and runs the checks
# in main/harness.c (../README.md).
#   run.sh <build folder>      (outside the repository; ESP-IDF v5.5.6 must be exported)
set -eu
[ $# -eq 1 ] || { echo "usage: $0 <build folder>" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
build=$1
mkdir -p "$build"
idf.py --preview -C "$here" -B "$build" -DIDF_TARGET=linux build >"$build/build.log" 2>&1 ||
    { tail -20 "$build/build.log" >&2; exit 1; }
"$build/nvs_setup_harness.elf"
