"""Lists what has code in the official firmware, for tools/notices/firmware-parts.txt.

    python3 -I tools/notices/firmware-parts.py <firmware build folder>

Reads the linker's map files of a firmware build (display-hub.map and bootloader/bootloader.map)
and prints, in order, each library with code or data in the image, and each of LVGL's bundled
libraries and built-in fonts that is in it. The CI firmware job compares this with
firmware-parts.txt: a change there means part 2 of THIRD-PARTY-NOTICES.txt (from
tools/notices/firmware.txt) must be looked at again, as firmware/THIRD_PARTY.md says.
"""

import pathlib
import re
import sys

# One input section of the map: its name (on the same line or the line before), its address, its
# size and the archive member it comes from.
SECTION = re.compile(r"^(?:\s+| \S+\s+)0x([0-9a-f]+)\s+0x([0-9a-f]+)\s+(\S+\.a)\((\S+)\)$", re.M)
# LVGL's folders for the libraries it bundles (Tiny TTF's wrapper is under src/font/), and its
# built-in fonts.
LVGL_BUNDLED = ("/src/libs/", "/src/stdlib/builtin/", "/src/font/tiny_ttf/")
LVGL_FONT = re.compile(r"^lv_font_[a-z_]+_[0-9]+[a-z_]*\.c\.obj$")


def kept(map_file: pathlib.Path) -> dict[str, set[str]]:
    """Each archive's members that have at least one byte placed in the image."""
    text = map_file.read_text(encoding="utf-8", errors="replace")
    body = text.split("Linker script and memory map", 1)[1]
    found: dict[str, set[str]] = {}
    for m in SECTION.finditer(body):
        addr, size, lib, member = m.groups()
        if int(addr, 16) and int(size, 16):
            found.setdefault(lib.replace("\\", "/").rsplit("/", 1)[-1], set()).add(member)
    return found


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: firmware-parts.py <firmware build folder>", file=sys.stderr)
        return 2
    build = pathlib.Path(sys.argv[1])
    lines = []
    for name, map_file in (("app", build / "display-hub.map"),
                           ("bootloader", build / "bootloader" / "bootloader.map")):
        found = kept(map_file)
        lines += [f"{name} {lib}" for lib in sorted(found)]
        if name == "app":
            members = found.get("liblvgl__lvgl.a", set())
            objects = build / "esp-idf" / "lvgl__lvgl"
            for member in sorted(members):
                paths = [p.as_posix() for p in objects.rglob(member)]
                if LVGL_FONT.match(member) or any(b in p for p in paths for b in LVGL_BUNDLED):
                    lines.append(f"app lvgl {member.removesuffix('.c.obj')}")
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())
