#!/usr/bin/env bash
# Checks that an mbedTLS client configured like ESP-IDF's (see client.c) can talk to hubd, and is
# refused when the certificate or name is wrong. Needs a C compiler, libmbedtls-dev (mbedTLS 3.6,
# as in ESP-IDF 5.x) and a built hubd.
#
#   cargo build -p hubd && bash tools/mbedtls-interop/run.sh [path/to/hubd]
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
hubd=${1:-${CARGO_TARGET_DIR:-$here/../../target}/debug/hubd}
[ -x "$hubd" ] || { echo "no hubd at $hubd; run cargo build -p hubd" >&2; exit 2; }
work=$(mktemp -d)
pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; rm -rf "$work"; }
trap cleanup EXIT

cc -O1 -Wall -Werror -o "$work/client" "$here/client.c" -lmbedtls -lmbedx509 -lmbedcrypto

start_hub() { # data-dir port
  "$hubd" serve --data-dir "$1" --listen "127.0.0.1:$2" >"$1.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 1 50); do
    [ -f "$1/identity/hub-cert.pem" ] && grep -q "serving HTTPS" "$1.log" && return 0
    sleep 0.1
  done
  cat "$1.log" >&2
  return 1
}
start_hub "$work/hub-a" 18443
start_hub "$work/hub-b" 18444
cert_a=$work/hub-a/identity/hub-cert.pem
cert_b=$work/hub-b/identity/hub-cert.pem
name=hub.display-hub.internal
failures=0

expect() { # description wanted-exit args...
  local what=$1 want=$2; shift 2
  local got=0 out
  out=$("$work/client" "$@" 2>&1) || got=$?
  if [ "$got" = "$want" ]; then
    echo "ok   $what"
    printf '%s\n' "$out" | sed 's/^/       /'
  else
    echo "FAIL $what (exit $got, wanted $want)"
    printf '%s\n' "$out" | sed 's/^/       /'
    failures=$((failures + 1))
  fi
}

expect "TLS 1.2 with the hub's certificate and name" 0 127.0.0.1 18443 "$cert_a" "$name" tls12 /v1/health
expect "TLS 1.3 with the hub's certificate and name" 0 127.0.0.1 18443 "$cert_a" "$name" tls13 /v1/health
expect "an authenticated route answers over mbedTLS (401 for a fake token)" 0 127.0.0.1 18443 "$cert_a" "$name" tls12 /v1/device/snapshot
expect "a different hub (another key) is refused" 2 127.0.0.1 18444 "$cert_a" "$name" tls12 /v1/health
expect "the wrong server name is refused" 2 127.0.0.1 18443 "$cert_a" raspberrypi.local tls12 /v1/health

[ "$failures" = 0 ] && echo "mbedTLS interop: all passed" || { echo "mbedTLS interop: $failures failed"; exit 1; }
