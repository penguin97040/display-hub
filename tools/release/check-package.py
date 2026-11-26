"""Checks the layout of a download that tools/release/package-app.sh or pi-tarball.sh packed.

    python3 -I tools/release/check-package.py <windows|macos|linux|pi> <version> <folder> [<firmware>]

<folder> holds the file, named as the release names it. It must hold one folder of the same name
(display-hub-pi for the Pi) with exactly the files that download ships: programs 0755, everything
else 0644 and folders 0755, every entry with the same time, owned by root in a tarball and in name
order in a zip. CI runs this on every push, the release workflow on what it publishes. Use relative
paths on Windows, where Python doesn't read Git Bash's /d/... paths.

Every download holds LICENSE and THIRD-PARTY-NOTICES.txt, the same bytes as the repository's (and
on macOS a second copy of the notices inside the app, in Display Hub.app/Contents/Resources/).

<firmware> is the official firmware package an app download must hold: under its own name in
firmware/ beside the app (Display Hub.app/Contents/Resources/firmware/ on macOS), where the app
looks for it, and the same bytes. Without it, the download must hold no firmware at all.
"""

import hashlib
import os
import pathlib
import sys
import tarfile
import zipfile

TOOLS = ["hub-send", "hub-firmware", "display-simulator", "usage-helper", "usage-helper-sync"]
# The Pi download's programs and scripts, besides what deploy/raspberry-pi/ holds as plain files.
PI_PROGRAMS = ["hubd", "hub-send", "install.sh", "display-hub-helper", "helpers/pi-status"]
APP = "Display Hub.app/Contents"
ROOT = pathlib.Path(__file__).resolve().parents[2]
NOTICES = "THIRD-PARTY-NOTICES.txt"
# Files that must be the repository's own, byte for byte: (where the repository has it, where the
# download has it).
COPIES = [("LICENSE", "LICENSE"), (NOTICES, NOTICES)]


def firmware_path(platform: str, firmware: str) -> str:
    """Where a download keeps the official firmware package, inside its folder."""
    folder = f"{APP}/Resources/firmware" if platform == "macos" else "firmware"
    return f"{folder}/{pathlib.Path(firmware).name}"


def expected(platform: str, version: str, firmware: str | None) -> tuple[str, str, dict[str, int], set[str]]:
    """The file's name, its folder, each file in the folder with its mode, and the folders."""
    if platform == "pi":
        deploy = ROOT / "deploy" / "raspberry-pi"
        files = {p.relative_to(deploy).as_posix(): 0o644 for p in deploy.rglob("*") if p.is_file()}
        dirs = {p.relative_to(deploy).as_posix() for p in deploy.rglob("*") if p.is_dir()}
        files.update({p: 0o755 for p in PI_PROGRAMS})
        files.update({"LICENSE": 0o644, NOTICES: 0o644})
        return f"display-hub-pi-arm64-{version}.tar.gz", "display-hub-pi", files, dirs
    exe = ".exe" if platform == "windows" else ""
    arch = {"windows": "windows-x64", "macos": "macos-arm64", "linux": "linux-x64"}[platform]
    top = f"display-hub-{arch}-{version}"
    files = {"LICENSE": 0o644, NOTICES: 0o644} | {t + exe: 0o755 for t in TOOLS}
    dirs: set[str] = set()
    if platform == "macos":
        files |= {
            f"{APP}/Info.plist": 0o644,
            f"{APP}/MacOS/hub-manager": 0o755,
            f"{APP}/_CodeSignature/CodeResources": 0o644,
            f"{APP}/Resources/{NOTICES}": 0o644,
        }
        dirs |= {"Display Hub.app", APP, f"{APP}/MacOS", f"{APP}/Resources", f"{APP}/_CodeSignature"}
    else:
        files["hub-manager" + exe] = 0o755
    if firmware is not None:
        inside = firmware_path(platform, firmware)
        files[inside] = 0o644
        parent = pathlib.PurePosixPath(inside).parent
        dirs.add(parent.as_posix())
    ext = ".zip" if platform in ("windows", "macos") else ".tar.gz"
    return top + ext, top, files, dirs


def entries(path: pathlib.Path) -> list[tuple[str, bool, int, object]]:
    """(name, is a folder, mode, time) for each entry, in the archive's order."""
    out = []
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as z:
            for i in z.infolist():
                if i.create_system != 3:
                    sys.exit(f"{i.filename}: not made with Unix modes")
                out.append((i.filename.rstrip("/"), i.is_dir(), (i.external_attr >> 16) & 0o7777, i.date_time))
        return out
    with tarfile.open(path, "r:gz") as t:
        for i in t.getmembers():
            if not (i.isfile() or i.isdir()):
                sys.exit(f"{i.name}: neither a file nor a folder")
            if (i.uid, i.gid, i.uname, i.gname) != (0, 0, "", ""):
                sys.exit(f"{i.name}: owned by {i.uid}:{i.gid} {i.uname}:{i.gname}, not root by number")
            out.append((i.name.rstrip("/"), i.isdir(), i.mode, i.mtime))
    return out


def content(path: pathlib.Path, entry: str) -> bytes:
    """One file's bytes, from the archive."""
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as z:
            return z.read(entry)
    with tarfile.open(path, "r:gz") as t:
        f = t.extractfile(entry)
        return f.read() if f else b""


def main() -> int:
    if len(sys.argv) not in (4, 5) or sys.argv[1] not in ("windows", "macos", "linux", "pi"):
        print("usage: check-package.py <windows|macos|linux|pi> <version> <folder> [<firmware package>]",
              file=sys.stderr)
        return 2
    platform, version, folder = sys.argv[1:4]
    firmware = sys.argv[4] if len(sys.argv) == 5 else None
    if firmware is not None and platform == "pi":
        print("the Pi download holds no firmware package", file=sys.stderr)
        return 2
    if firmware is not None and not pathlib.Path(firmware).is_file():
        print(f"no firmware package at {firmware}", file=sys.stderr)
        return 1
    name, top, files, dirs = expected(platform, version, firmware)
    path = pathlib.Path(folder) / name
    if not path.is_file():
        print(f"no {path}", file=sys.stderr)
        return 1
    found = entries(path)
    problems = []
    got_files, got_dirs, times = {}, set(), set()
    for entry, is_dir, mode, when in found:
        if entry != top and not entry.startswith(top + "/"):
            problems.append(f"{entry} is outside {top}/")
            continue
        rel = entry[len(top) + 1:]
        if ".." in rel.split("/") or os.path.isabs(rel):
            problems.append(f"{entry}: an odd path")
        times.add(when)
        if is_dir:
            if mode != 0o755:
                problems.append(f"{entry}/ is {mode:o}, not 755")
            if rel:
                got_dirs.add(rel)
        else:
            got_files[rel] = mode
    for rel in sorted(files.keys() - got_files.keys()):
        problems.append(f"{top}/{rel} is missing")
    for rel in sorted(got_files.keys() - files.keys()):
        problems.append(f"{top}/{rel} shouldn't be there")
    for rel in sorted(files.keys() & got_files.keys()):
        if got_files[rel] != files[rel]:
            problems.append(f"{top}/{rel} is {got_files[rel]:o}, not {files[rel]:o}")
    # A zip needn't list folders, but any it lists must be ones the download has.
    for rel in sorted(got_dirs - dirs):
        problems.append(f"{top}/{rel}/ shouldn't be there")
    if len(times) != 1:
        problems.append(f"the entries have {len(times)} different times, not one")
    if path.suffix == ".zip":
        names = [e[0] + ("/" if e[1] else "") for e in found]
        if names != sorted(names):
            problems.append("the zip's entries aren't in name order")
    if not problems:
        copies = COPIES + ([(NOTICES, f"{APP}/Resources/{NOTICES}")] if platform == "macos" else [])
        for ours, inside in copies:
            if content(path, f"{top}/{inside}") != (ROOT / ours).read_bytes():
                problems.append(f"{top}/{inside} isn't the same as the repository's {ours}")
    said = ""
    if firmware is not None and not problems:
        inside = firmware_path(platform, firmware)
        want = pathlib.Path(firmware).read_bytes()
        if content(path, f"{top}/{inside}") != want:
            problems.append(f"{top}/{inside} isn't the same as {firmware}")
        said = f", with the firmware package {inside} (sha256 {hashlib.sha256(want).hexdigest()})"
    if problems:
        print(f"{path}:", *problems, sep="\n  ", file=sys.stderr)
        return 1
    print(f"{path}: {len(got_files)} files as expected{said}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
