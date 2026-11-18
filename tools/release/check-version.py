"""Checks a release tag against the workspace's versions, and says what it releases.

    python3 -I tools/release/check-version.py v0.1.0

The tag is v<version>, optionally with -rc.N or -test.N after it, and <version> must be the version
of every program the release ships (each crate's Cargo.toml; the workspace has no version of its
own). Prints version=, prerelease=, preview= and name= lines for $GITHUB_OUTPUT, or fails saying
why. A preview is a 0.x version or a tagged suffix: its title says so, and so do its notes
(tools/release/release-notes.sh).
"""

import pathlib
import re
import sys
import tomllib

SHIPPED = [
    "apps/hubd",
    "apps/hub-manager",
    "apps/hub-send",
    "apps/hub-firmware",
    "apps/usage-helper",
    "tools/display-simulator",
]


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check-version.py <tag>", file=sys.stderr)
        return 2
    tag = sys.argv[1]
    m = re.fullmatch(r"v([0-9]+\.[0-9]+\.[0-9]+)(-(?:rc|test)\.[0-9]+)?", tag)
    if not m:
        print(f"the tag {tag!r} isn't v<major>.<minor>.<patch>, with -rc.N or -test.N after it if any",
              file=sys.stderr)
        return 1
    core, suffix = m.group(1), m.group(2) or ""
    root = pathlib.Path(__file__).resolve().parents[2]
    wrong = []
    for crate in SHIPPED:
        with open(root / crate / "Cargo.toml", "rb") as f:
            version = tomllib.load(f)["package"]["version"]
        if version != core:
            wrong.append(f"{crate} is {version}")
    if wrong:
        print(f"the tag {tag} says {core}, but " + ", ".join(wrong)
              + ". Change the versions in Cargo.toml, or tag again.", file=sys.stderr)
        return 1
    version = core + suffix
    preview = core.startswith("0.") or bool(suffix)
    print(f"version={version}")
    print(f"prerelease={'true' if suffix else 'false'}")
    print(f"preview={'true' if preview else 'false'}")
    print(f"name=Display Hub {version}{' (preview)' if preview else ''}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
