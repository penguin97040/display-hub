#!/usr/bin/env bash
# Writes THIRD-PARTY-NOTICES.txt at the top of the repository: the licences and copyright notices
# of the work by others that Display Hub's downloads carry. Every download gets the same file.
#
#   bash tools/notices/generate.sh           writes it
#   bash tools/notices/generate.sh --check   fails if the committed file isn't what it would write
#
# It is put together from:
#   1. the Geist font's licence, assets/fonts/OFL-Geist.txt;
#   2. the official firmware's parts, tools/notices/firmware.txt (written by hand from a release
#      build's map file: firmware/THIRD_PARTY.md says how, and when to do it again);
#   3. the Rust crates, from cargo-about with tools/notices/about.toml and rust-crates.hbs.
#
# cargo-about runs offline (--frozen): it reads only the crates already downloaded for this
# Cargo.lock, never the network. It must be exactly the version below, as CI installs it:
#   cargo install --locked --features cli --version 0.9.3 cargo-about
# It also needs Python 3.11 or newer. Give cargo-about's path in $CARGO_ABOUT if it isn't on PATH. The same Cargo.lock gives the same file.
set -euo pipefail

want_about=0.9.3
check=false
case ${1:-} in
  "") ;;
  --check) check=true ;;
  *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac
root=$(cd "$(dirname "$0")/../.." && pwd)
here=$root/tools/notices
about=${CARGO_ABOUT:-cargo-about}
got_about=$("$about" --version 2>/dev/null | awk 'NR == 1 && $1 == "cargo-about" {print $2}') || true
[ "$got_about" = "$want_about" ] || {
  echo "needs cargo-about $want_about, found ${got_about:-none} ($about)" >&2
  exit 2
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
"$about" generate --frozen --workspace --fail --manifest-path "$root/Cargo.toml" \
  --config "$here/about.toml" --output-file "$work/rust.txt" "$here/rust-crates.hbs"

python3 -I -c 'import sys; sys.exit(sys.version_info < (3, 11))' || {
  echo "needs Python 3.11 or newer (for tomllib)" >&2
  exit 2
}
python3 -I - "$root" "$work/rust.txt" "$work/notices.txt" <<'PY'
import pathlib, sys, tomllib

root, rust, dest = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3])

# about.toml must accept exactly what deny.toml allows, so the two can't drift apart.
deny = tomllib.loads((root / "deny.toml").read_text(encoding="utf-8"))["licenses"]
about = tomllib.loads((root / "tools/notices/about.toml").read_text(encoding="utf-8"))
if about["accepted"] != deny["allow"]:
    sys.exit("tools/notices/about.toml's accepted list isn't deny.toml's allow list, in order")
mpl = sorted(e["crate"] for e in deny["exceptions"] if e["allow"] == ["MPL-2.0"] and e["crate"] != "cbindgen")
mpl_names = ", ".join(mpl[:-1]) + " and " + mpl[-1] if len(mpl) > 1 else "".join(mpl)
if sorted(k for k, v in about.items() if isinstance(v, dict) and v.get("accepted") == ["MPL-2.0"]) != mpl:
    sys.exit("tools/notices/about.toml's MPL-2.0 crates aren't deny.toml's shipped ones")

def text(path):
    return path.read_text(encoding="utf-8").replace("\r\n", "\n").replace("\r", "\n").strip("\n")

RULE = "=" * 96
def part(title):
    return f"\n\n{RULE}\n{title}\n{RULE}\n\n"

out = """Display Hub: third-party notices
================================

Display Hub's own code is under the MIT licence, in the LICENSE file beside this one. Display Hub
also uses work by other people, under their own licences. This file lists that work, with the
licence texts and copyright notices those licences ask to be passed on.

Every download carries this same file, so not everything in it is in every download:

  The desktop app and the tools (Windows, macOS and Linux) hold the parts in 1, 2 and 3.
  The Raspberry Pi download (hubd and hub-send) holds only some of the crates in 3.

  1. The Geist font
  2. The official display firmware
  3. Rust crates

Display Hub's source code: https://github.com/penguin97040/display-hub"""

out += part("1. The Geist font")
out += """The desktop app draws its text in Geist and Geist Mono, and the display firmware in Geist, from
https://github.com/vercel/geist-font. The font files are used unchanged. Their licence, the SIL
Open Font License 1.1, is below. Montserrat and Font Awesome, in part 2, are under it too.

"""
out += text(root / "assets/fonts/OFL-Geist.txt")

out += part("2. The official display firmware")
out += """The app downloads include the official firmware for the Waveshare ESP32-S3-Touch-AMOLED-1.8
(V2) display: the .dhfw file in the firmware folder. It holds the firmware, its bootloader and its
partition table, built with Espressif's ESP-IDF v5.5.6 and the components below. Display Hub's own
code in it is under the MIT licence. Only the parts with code in the firmware are listed, as the
linker's record of a release build (its map file) shows them.

"""
out += text(root / "tools/notices/firmware.txt")

out += part("3. Rust crates")
out += """The programs are built from Rust crates, most of them from crates.io: hub-manager, hub-send,
hub-firmware, display-simulator, usage-helper and usage-helper-sync in the app downloads, and hubd
and hub-send in the Raspberry Pi download. This part lists every crate any of them uses on the
systems the downloads are for, with its licence. Crates used only to build or test Display Hub
aren't included (crates that only run while it is compiled, such as derive macros, are listed
all the same), nor are Display Hub's own.

Every program also holds Rust's standard library (std, core and alloc), from
https://github.com/rust-lang/rust, under the MIT licence or the Apache License 2.0, at the user's
choice. The Apache License 2.0's text is below. The MIT licence's notice is:

  Copyright (c) The Rust Project Contributors

  Permission is hereby granted, free of charge, to any person obtaining a copy of this software
  and associated documentation files (the "Software"), to deal in the Software without
  restriction, including without limitation the rights to use, copy, modify, merge, publish,
  distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
  Software is furnished to do so, subject to the following conditions:

  The above copyright notice and this permission notice shall be included in all copies or
  substantial portions of the Software.

  THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
  BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
  NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
  DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
  OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
""" + (f"""
The crates under the Mozilla Public License 2.0 (MPL-2.0) are {mpl_names}.
They are used unmodified. The MPL asks that their source code stays available: it is on crates.io
(https://crates.io/crates/<name>) and in the repository listed with each crate in the MPL-2.0
section below.
""" if mpl else "") + """
gpui-pre-windows 0.3.11 (Apache License 2.0), GPUI's Windows support, is used in a changed copy:
it compiles the app's Windows shaders when the app first starts, not while it is built. Display
Hub's source code has the copy in vendor/gpui-pre-windows, and vendor/README.md says what changed.

"""
out += text(rust)
out += "\n\n" + RULE + "\nThis file is made by tools/notices/generate.sh in Display Hub's source code.\n"

lines = [line.rstrip() for line in out.split("\n")]
dest.write_text("\n".join(lines).rstrip("\n") + "\n", encoding="utf-8")
PY

target=$root/THIRD-PARTY-NOTICES.txt
if $check; then
  if ! cmp -s "$work/notices.txt" "$target"; then
    diff -u "$target" "$work/notices.txt" | head -40 >&2 || true
    echo "THIRD-PARTY-NOTICES.txt is out of date: run bash tools/notices/generate.sh and commit it" >&2
    exit 1
  fi
  echo "THIRD-PARTY-NOTICES.txt is up to date"
else
  cp "$work/notices.txt" "$target"
  echo "$target"
fi
