"""Checks dh_core against the protocol's conformance files. Standard library only.

  run_conformance.py vectors <dh_conform> <protocol/v1/vectors>
      Every vector file in the folder must give exactly its "expect".
  run_conformance.py examples <dh_conform> <protocol/v1/examples>
      Every example in valid/ and invalid/ must be accepted, tolerated, shown as a card or refused
      as docs/protocol-v1.md, "Conformance", says (crates/hub-protocol/tests/conformance.rs has
      the same table).
  run_conformance.py firmware <dh_fw_conform> <protocol/v1/firmware>
      Every firmware update vector must give exactly its "expect", whatever size of piece a
      download's body arrives in.
"""

import json
import pathlib
import subprocess
import sys

# docs/protocol-v1.md, "Conformance": tolerated and shown.
TOLERATED = {
    "extra-field",
    "shape-extra-field",
    "unknown-quality",
    "missing-used-percent",
    "firmware-update-bad-offer-id",
}
# A card for one page, with the others still shown.
CARDS = {
    "unknown-kind": "unknown_kind",
    "custom-kind-bad-version": "unknown_kind",
    "page-too-large": "too_large",
    "custom-kind-not-object": "bad_data",
    "custom-data-too-deep": "bad_data",
    "shape-bad-data": "bad_data",
    "big-number-decimals": "bad_data",
    "big-number-too-big": "bad_data",
    "list-too-many-items": "bad_data",
    "meter-duplicate-ids": "bad_data",
    "data-missing": "bad_data",
    "unavailable-with-data": "bad_data",
    "percent-as-string": "bad_data",
    "percent-negative": "bad_data",
    "percent-over-100": "bad_data",
    "too-many-metrics": "bad_data",
}
# Refused as from another protocol version, rather than as unreadable.
INCOMPATIBLE = {"wrong-version"}


def run(cli, args):
    out = subprocess.run([cli, *args], check=True, capture_output=True, text=True).stdout
    return [json.loads(line) for line in out.split("\n") if line]


def check_vectors(cli, folder):
    files = sorted(pathlib.Path(folder).glob("*.json"))
    if not files:
        print(f"no vectors in {folder}")
        return 1
    got = run(cli, ["vector", *map(str, files)])
    failures = 0
    for f, g in zip(files, got):
        want = json.loads(f.read_text(encoding="utf-8"))["expect"]
        if g != want:
            failures += 1
            print(f"FAIL {f.name}\n  expected {json.dumps(want)}\n  got      {json.dumps(g)}")
    print(f"vectors: {len(files) - failures} of {len(files)} pass")
    return 1 if failures or len(got) != len(files) else 0


# How many bytes of a download's body the display is given at a time: single bytes, sizes that
# straddle the 137-byte header and each vector's manifest, the reference's own 97, and everything
# at once.
PIECES = (1, 2, 3, 7, 64, 97, 136, 137, 138, 255, 1000, 4096, 1 << 20)


def check_firmware(cli, folder):
    files = sorted(pathlib.Path(folder).glob("*.json"))
    if not files:
        print(f"no firmware vectors in {folder}")
        return 1
    wants = [json.loads(f.read_text(encoding="utf-8"))["expect"] for f in files]
    failed = set()
    for piece in PIECES:
        got = run(cli, [str(piece), *map(str, files)])
        if len(got) != len(files):
            print(f"FAIL: {len(got)} answers for {len(files)} vectors")
            return 1
        for f, want, g in zip(files, wants, got):
            if g != want:
                failed.add(f.name)
                print(
                    f"FAIL {f.name} in pieces of {piece}\n"
                    f"  expected {json.dumps(want)}\n  got      {json.dumps(g)}"
                )
    print(
        f"firmware vectors: {len(files) - len(failed)} of {len(files)} pass, "
        f"each at {len(PIECES)} piece sizes"
    )
    return 1 if failed else 0


def problem_with(kind, name, body, got):
    pages = got["pages"]
    if kind == "valid":
        if got["outcome"] != "show":
            return f"refused ({got['outcome']})"
        # The reference display, like this one by default, draws no custom kinds.
        for wire, page in zip(body["pages"], pages):
            custom = "/" in wire["kind"]
            if custom and page["cant_show_reason"] != "unknown_kind":
                return f"{page['id']}: a custom kind should be an unknown_kind card"
            if not custom and page["show"] != "show":
                return f"{page['id']}: not shown ({page['cant_show_reason']})"
        return None
    if name in INCOMPATIBLE:
        return None if got["outcome"] == "incompatible" else f"gave {got['outcome']}"
    if name in TOLERATED:
        if got["outcome"] != "show" or any(p["show"] != "show" for p in pages):
            return "not tolerated"
        if name == "unknown-quality" and (
            pages[0]["quality_label"] != "UNAVAILABLE" or pages[0]["content"] is not None
        ):
            return "an unknown quality must read as unavailable"
        if name == "missing-used-percent" and pages[0]["content"]["items"][0]["percent"] is not None:
            return "a missing percent must read as unknown"
        return None
    if name in CARDS:
        if got["outcome"] != "show":
            return f"refused ({got['outcome']})"
        cards = [p["cant_show_reason"] for p in pages if p["show"] != "show"]
        if cards != [CARDS[name]]:
            return f"cards {cards}, expected one {CARDS[name]}"
        if not any(p["show"] == "show" for p in pages):
            return "no other page shows"
        return None
    return None if got["outcome"] == "unreadable" else f"not refused ({got['outcome']})"


def check_examples(cli, folder):
    failures = total = 0
    for kind in ("valid", "invalid"):
        files = sorted((pathlib.Path(folder) / kind).glob("*.json"))
        if not files:
            print(f"no {kind} examples")
            return 1
        got = run(cli, ["body", "200", "0", *map(str, files)])
        for f, g in zip(files, got):
            total += 1
            body = json.loads(f.read_text(encoding="utf-8"))
            why = problem_with(kind, f.stem, body, g)
            if why:
                failures += 1
                print(f"FAIL {kind}/{f.name}: {why}")
    print(f"examples: {total - failures} of {total} pass")
    return 1 if failures else 0


def main():
    checks = {"vectors": check_vectors, "examples": check_examples, "firmware": check_firmware}
    if len(sys.argv) != 4 or sys.argv[1] not in checks:
        print(__doc__)
        return 2
    mode, cli, folder = sys.argv[1:]
    return checks[mode](cli, folder)


if __name__ == "__main__":
    sys.exit(main())
