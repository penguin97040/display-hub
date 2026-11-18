#!/usr/bin/env bash
# Fills in .github/release-notes.md for one release and prints it. The release workflow runs it for
# the draft; CI runs it with stand-in values on every push, so a stand-in it misses shows up there.
#
#   bash tools/release/release-notes.sh <tag> <version> <preview> <repo-url> <pi-tarball> <pi-url> <pi-sha256>
#
# <preview> is true or false, as tools/release/check-version.py says: a preview's notes open with
# the line that says so, anything else's with the line marked {{STABLE}}.
set -euo pipefail

[ $# -eq 7 ] || { echo "usage: $0 <tag> <version> <preview> <repo-url> <pi-tarball> <pi-url> <pi-sha256>" >&2; exit 2; }
tag=$1 version=$2 preview=$3 repo=$4 tarball=$5 url=$6 sha=$7
root=$(cd "$(dirname "$0")/../.." && pwd)
case $preview in
  true) keep=PREVIEW drop=STABLE ;;
  false) keep=STABLE drop=PREVIEW ;;
  *) echo "preview must be true or false, not $preview" >&2; exit 2 ;;
esac
# Every value is one the workflow made; none may hold the "|" that sed splits on, the "&" or "\" it reads
# specially, or a new line.
for v in "$tag" "$version" "$repo" "$tarball" "$url" "$sha"; do
  case $v in *'|'* | *'&'* | *\\* | *$'\n'* | '') echo "not a value for the notes: $v" >&2; exit 2 ;; esac
done

notes=$(sed -e "/^{{$drop}}/d" -e "s/^{{$keep}}//" \
  -e "s|{{VERSION}}|$version|g" -e "s|{{TAG}}|$tag|g" -e "s|{{REPO_URL}}|$repo|g" \
  -e "s|{{PI_TARBALL}}|$tarball|g" -e "s|{{PI_URL}}|$url|g" -e "s|{{PI_SHA256}}|$sha|g" \
  "$root/.github/release-notes.md")
if grep -n '{{' <<<"$notes" >&2; then
  echo "a stand-in above wasn't filled in" >&2
  exit 1
fi
printf '%s\n' "$notes"
