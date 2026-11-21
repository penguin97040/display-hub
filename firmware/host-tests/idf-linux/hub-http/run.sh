#!/bin/sh
# Builds main/hub_http.c for a computer and runs it against a stand-in hub (../README.md).
#   run.sh <build folder>      (outside the repository; ESP-IDF v5.5.6 must be exported)
# Takes about five minutes: seven of the cases wait out the 30-second limit.
set -eu
[ $# -eq 1 ] || { echo "usage: $0 <build folder>" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
build=$1
mkdir -p "$build/certs"
# Throwaway test certificates, made afresh for each run and never kept in the repository: two
# for the hub's name under different keys, and one for another name.
for name in hub other wrongname; do
    cn=hub.display-hub.internal
    [ "$name" = wrongname ] && cn=someone.else.internal
    openssl ecparam -name prime256v1 -genkey -noout -out "$build/certs/$name.key"
    openssl req -new -x509 -key "$build/certs/$name.key" -out "$build/certs/$name.pem" -days 2 \
        -subj "/CN=$cn" -addext "subjectAltName=DNS:$cn" 2>/dev/null
done
idf.py --preview -C "$here" -B "$build" -DIDF_TARGET=linux build >"$build/build.log" 2>&1 ||
    { tail -20 "$build/build.log" >&2; exit 1; }
python3 "$here/server.py" "$build/certs" >"$build/server.log" 2>&1 &
server=$!
trap 'kill $server 2>/dev/null || true' EXIT
sleep 2
# (ESP-IDF's HTTP client also logs that it has no event loop to post to here; that isn't a failure.)
CERTS="$build/certs" "$build/hub_http_harness.elf" 2>&1 | grep -v "Failed to post http_client event"
