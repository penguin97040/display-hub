"""Checks the official firmware package against a fresh build of the firmware and the official keys.

    python3 -I tools/release/check-official-firmware.py --dir firmware/official --build <build folder>
        --hub-firmware <hub-firmware program> [--app-keys <firmware.rs>] [--version <version>]
        [--none-ok] [--stale-ok]

<dir> holds keys.txt (the official public keys) and exactly one package, named
display-hub-waveshare-amoled-1.8-<version>.dhfw. <build folder> is an ESP-IDF build of firmware/, or
the same files as CI keeps them: flasher_args.json, display-hub.bin, bootloader/bootloader.bin,
partition_table/partition-table.bin and ota_data_initial.bin.

The owner signs the package on their own computer, so nothing here trusts it. It passes only if:

- `hub-firmware inspect` accepts it (well formed, every hash and the signature good);
- its app image and each USB-install part are byte for byte what the build made (SHA-256 and size),
  at the build's offsets, for the build's chip and flash size;
- its version is the built firmware's, is in its file name, and is --version when that is given;
- its device type and hardware revision are the official board's;
- its signer is one of the keys in keys.txt, and its key set is exactly those keys;
- with --app-keys, the app's OFFICIAL_KEYS (in apps/hub-manager/src/firmware.rs) are those keys'
  ids, so the app that ships with the package names it official.

--none-ok passes, saying so, when the folder has no package (CI, before the first one exists).
--stale-ok turns a difference from the build (the second and third points, apart from the file's
name) into a warning: CI uses it, so the firmware can change between releases. A release uses neither.

Prints what it checked, or fails saying why. Standard library only.
"""

import argparse
import hashlib
import json
import os
import pathlib
import re
import struct
import subprocess
import sys

DEVICE_TYPE = "display-hub-waveshare-amoled-1.8"
HARDWARE = ["waveshare-amoled-1.8-v2"]
CHIP = "esp32s3"
FLASH_SIZE = "16MB"
MIN_FLASH = 16 * 1024 * 1024
# The build's name for each USB-install part, and the package's.
PARTS = [("bootloader", "bootloader"), ("partition-table", "partition-table"), ("otadata", "otadata")]
KEY_RE = re.compile(r"04[0-9a-f]{128}")
VERSION_RE = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?")


class Failed(Exception):
    """The package can't be released. The message says why, plainly."""


def sha256(path: pathlib.Path) -> tuple[str, int]:
    """A file's SHA-256 and size."""
    h = hashlib.sha256()
    size = 0
    with open(path, "rb") as f:
        while chunk := f.read(1 << 16):
            h.update(chunk)
            size += len(chunk)
    return h.hexdigest(), size


def official_keys(path: pathlib.Path) -> list[str]:
    """The public keys in keys.txt: 130 hex digits each, one to a line, # starts a comment."""
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        raise Failed(f"couldn't read the official keys in {path}: {e}") from e
    keys = []
    for n, line in enumerate(text.splitlines(), 1):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        if not KEY_RE.fullmatch(line):
            raise Failed(f"{path}, line {n}: not a public key (130 lower-case hex digits starting 04)")
        if line in keys:
            raise Failed(f"{path}, line {n}: this key is listed twice")
        keys.append(line)
    return keys


def key_id(public_hex: str) -> str:
    """A key's id: the SHA-256 of its 65 bytes, as the package library makes it."""
    return hashlib.sha256(bytes.fromhex(public_hex)).hexdigest()


def fingerprint(kid: str) -> str:
    return "-".join(kid[i:i + 4] for i in range(0, 32, 4))


def app_keys(path: pathlib.Path) -> list[str]:
    """The key ids in the app's OFFICIAL_KEYS, read from its source."""
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        raise Failed(f"couldn't read the app's keys in {path}: {e}") from e
    found = re.findall(r"^pub const OFFICIAL_KEYS: &\[&str\] = &\[(.*?)\];", text, re.MULTILINE | re.DOTALL)
    if len(found) != 1:
        raise Failed(f"{path} doesn't have one OFFICIAL_KEYS list this check can read")
    body = re.sub(r"//[^\n]*", "", found[0])
    ids = re.findall(r'"([^"]*)"', body)
    if re.sub(r'"[^"]*"', "", body).replace(",", "").strip() or not all(re.fullmatch(r"[0-9a-f]{64}", i) for i in ids):
        raise Failed(f"{path}: OFFICIAL_KEYS must be a plain list of key ids (64 lower-case hex digits each)")
    return ids


def the_package(folder: pathlib.Path) -> pathlib.Path | None:
    """The one package in the folder, or None when there is none."""
    if not folder.is_dir():
        raise Failed(f"{folder} isn't a folder")
    found = sorted(p for p in folder.iterdir() if p.suffix == ".dhfw")
    if len(found) > 1:
        raise Failed(f"{folder} holds {len(found)} packages ({', '.join(p.name for p in found)}): "
                     "keep only the one for this release")
    if not found:
        return None
    if found[0].is_symlink() or not found[0].is_file():
        raise Failed(f"{found[0]} isn't a plain file")
    return found[0]


def inspect(tool: str, package: pathlib.Path) -> tuple[dict, str]:
    """The manifest and the signer's key id, from `hub-firmware inspect`, which checks the package."""
    try:
        r = subprocess.run([tool, "inspect", str(package)], capture_output=True, text=True, encoding="utf-8",
                           timeout=120, check=False)
    except (OSError, subprocess.SubprocessError) as e:
        raise Failed(f"couldn't run {tool}: {e}") from e
    if r.returncode != 0:
        raise Failed(f"hub-firmware refuses the package: {r.stderr.strip() or r.stdout.strip()}")
    try:
        manifest, end = json.JSONDecoder().raw_decode(r.stdout)
    except ValueError as e:
        raise Failed(f"couldn't read what hub-firmware inspect printed: {e}") from e
    if not isinstance(manifest, dict):
        raise Failed("hub-firmware inspect didn't print a manifest")
    ids = re.findall(r"^Signer key id: ([0-9a-f]{64})$", r.stdout[end:], re.MULTILINE)
    if len(ids) != 1:
        raise Failed("hub-firmware inspect didn't print one 'Signer key id:' line; is it an older hub-firmware?")
    return manifest, ids[0]


class Build:
    """What a build folder says the firmware is."""

    def __init__(self, folder: pathlib.Path):
        try:
            with open(folder / "flasher_args.json", encoding="utf-8") as f:
                args = json.load(f)
        except (OSError, ValueError) as e:
            raise Failed(f"couldn't read {folder / 'flasher_args.json'}: {e}; is it a firmware build folder?") from e
        self.chip = args.get("extra_esptool_args", {}).get("chip")
        self.flash_size = args.get("flash_settings", {}).get("flash_size")

        def entry(name: str) -> tuple[int, pathlib.Path]:
            e = args.get(name)
            if not isinstance(e, dict) or "offset" not in e or "file" not in e:
                raise Failed(f"the build's flasher_args.json has no {name!r} entry")
            if e.get("encrypted", "false") != "false":
                raise Failed(f"the build marks {name} as encrypted; flash encryption is meant to be off")
            path = folder / e["file"]
            if not path.is_file():
                raise Failed(f"the build's {name} file {path} is missing")
            return int(e["offset"], 16), path

        self.app_offset, self.app = entry("app")
        self.app_sha256, self.app_size = sha256(self.app)
        # Name, offset, SHA-256 and size of each part, in flash order, as the package lists them.
        parts = []
        for theirs, ours in PARTS:
            offset, path = entry(theirs)
            parts.append((offset, ours, *sha256(path)))
        self.parts = [
            {"name": name, "offset": offset, "sha256": digest, "size": size}
            for offset, name, digest, size in sorted(parts)
        ]
        self.version = self._version()

    def _version(self) -> str:
        """From the image's esp_app_desc_t: a 24-byte image header and an 8-byte segment header,
        then the description, whose 32-byte version field starts 16 bytes in."""
        with open(self.app, "rb") as f:
            head = f.read(80)
        if len(head) < 80 or head[0] != 0xE9 or struct.unpack_from("<I", head, 32)[0] != 0xABCD5432:
            raise Failed(f"{self.app} isn't an ESP-IDF app image with a description")
        try:
            version = head[48:80].split(b"\0", 1)[0].decode("ascii")
        except UnicodeDecodeError:
            version = ""
        if not VERSION_RE.fullmatch(version):
            raise Failed(f"the built firmware's version {version!r} isn't a version")
        return version


def check(args: argparse.Namespace) -> int:
    folder = pathlib.Path(args.dir)
    package = the_package(folder)
    if package is None:
        if args.none_ok:
            print(f"{folder} has no official package yet, so there is nothing to check.")
            return 0
        raise Failed(f"{folder} has no official package (a .dhfw file). The owner signs one for each release "
                     "and commits it there: see firmware/official/README.md")
    keys = official_keys(folder / "keys.txt")
    if not keys:
        raise Failed(f"{folder / 'keys.txt'} lists no official keys, so no package can be official")
    if args.app_keys is not None:
        built_in = app_keys(pathlib.Path(args.app_keys))
        if built_in != [key_id(k) for k in keys]:
            raise Failed(f"the app's OFFICIAL_KEYS ({args.app_keys}) aren't the ids of the keys in "
                         f"{folder / 'keys.txt'}, in the same order, so the app wouldn't name this firmware "
                         "official. They should be:\n  " + "\n  ".join(f'"{key_id(k)}",' for k in keys))
    manifest, signer = inspect(args.hub_firmware, package)
    build = Build(pathlib.Path(args.build))

    wrong = []  # never allowed
    stale = []  # the package and the build differ: a warning with --stale-ok

    def same(problems: list[str], what: str, got: object, want: object) -> None:
        if got != want:
            problems.append(f"{what} is {got!r}, not {want!r}")

    # Who signed it, and whom displays will trust.
    ids = {key_id(k): k for k in keys}
    if signer not in ids:
        wrong.append(f"it is signed by the key {fingerprint(signer)}, which isn't in keys.txt "
                     f"({', '.join(fingerprint(i) for i in ids)})")
    key_set = manifest.get("key_set")
    if not isinstance(key_set, list):
        wrong.append("it has no key set, so displays wouldn't be told to trust the official keys")
    else:
        for k in key_set:
            if k not in keys:
                wrong.append(f"its key set has a key that isn't in keys.txt ({fingerprint(key_id(str(k)))})"
                             if isinstance(k, str) and KEY_RE.fullmatch(k)
                             else "its key set has something that isn't a public key")
        for k in keys:
            if k not in key_set:
                wrong.append(f"its key set leaves out an official key ({fingerprint(key_id(k))})")

    # What it is for.
    same(wrong, "its device type", manifest.get("device_type"), DEVICE_TYPE)
    same(wrong, "its hardware", manifest.get("hardware_revisions"), HARDWARE)
    version = manifest.get("firmware_version")
    same(wrong, "the file's name", package.name, f"{DEVICE_TYPE}-{version}.dhfw")
    if args.version is not None:
        same(wrong, "its version", version, args.version)
    same(wrong, "the build's chip", build.chip, CHIP)
    same(wrong, "the build's flash size", build.flash_size, FLASH_SIZE)

    # The same bytes as the build.
    same(stale, "its version", version, build.version)
    same(stale, "the app image's SHA-256", manifest.get("image_sha256"), build.app_sha256)
    same(stale, "the app image's size", manifest.get("image_size"), build.app_size)
    usb = manifest.get("usb_install")
    if not isinstance(usb, dict):
        wrong.append("it has no USB-install parts, so it can't do a first install")
    else:
        same(wrong, "its chip", usb.get("chip"), CHIP)
        same(wrong, "its smallest flash", usb.get("min_flash_bytes"), MIN_FLASH)
        same(stale, "its app offset", usb.get("app_offset"), build.app_offset)
        got = usb.get("parts")
        got = got if isinstance(got, list) else []
        same(stale, "its USB-install parts", [p.get("name") if isinstance(p, dict) else p for p in got],
             [p["name"] for p in build.parts])
        for theirs, ours in zip(got, build.parts):
            if not isinstance(theirs, dict) or theirs.get("name") != ours["name"]:
                continue
            for field in ("offset", "sha256", "size"):
                same(stale, f"the {ours['name']}'s {field}", theirs.get(field), ours[field])

    if stale and args.stale_ok:
        lines = "; ".join(stale)
        note = (f"The official package {package.name} isn't this firmware any more ({lines}). That is fine "
                "between releases; a release needs a package signed from its own build.")
        print(f"::warning::{note}" if os.environ.get("GITHUB_ACTIONS") == "true" else f"warning: {note}")
    elif stale:
        wrong.append("it isn't this build of the firmware: " + "; ".join(stale)
                     + ". Sign a package from a clean build of this commit")
    if wrong:
        raise Failed(f"{package} can't be the official firmware:\n  " + "\n  ".join(wrong))

    print(f"{package}: {DEVICE_TYPE} {version} for {', '.join(HARDWARE)}")
    print(f"  signed by {fingerprint(signer)}; displays will trust {', '.join(fingerprint(i) for i in ids)}")
    if not stale:
        print(f"  app image {build.app_sha256} ({build.app_size} bytes) at 0x{build.app_offset:x}, as built")
        for p in build.parts:
            print(f"  {p['name']} {p['sha256']} ({p['size']} bytes) at 0x{p['offset']:x}, as built")
    print(f"  sha256 {sha256(package)[0]}")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="Checks the official firmware package against a fresh build.")
    ap.add_argument("--dir", required=True, help="the folder with keys.txt and the one package")
    ap.add_argument("--build", required=True, help="a build folder of firmware/")
    ap.add_argument("--hub-firmware", required=True, help="the hub-firmware program")
    ap.add_argument("--app-keys", help="the app's source file with OFFICIAL_KEYS, to check against keys.txt")
    ap.add_argument("--version", help="the version the package must be")
    ap.add_argument("--none-ok", action="store_true", help="pass when there is no package")
    ap.add_argument("--stale-ok", action="store_true", help="only warn when the package isn't this build")
    args = ap.parse_args()
    try:
        return check(args)
    except Failed as e:
        print(f"check-official-firmware.py: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
