"""Writes the seed inputs for fuzz_hub from the device examples and a few snapshots. Standard
library only.

  python3 -I firmware/fuzz/make_hub_corpus.py protocol/v1 firmware/fuzz/corpus

Run it again when the examples change. It writes only files whose content isn't there already, so
running it with nothing to change changes nothing.

Each input starts with one byte saying what the rest is (see fuzz_hub.c): an enrolment response,
an error answer after its two-byte status, a stored device ID and token, an enrolment secret, or
two snapshot bodies after the first one's two-byte length.
"""

import hashlib
import json
import pathlib
import struct
import sys

CODES = [
    (401, "unauthorised"), (403, "forbidden"), (401, "token_revoked"), (401, "unknown_device"),
    (409, "enrolment_used"), (401, "enrolment_expired"), (426, "incompatible_version"),
    (400, "bad_request"), (413, "payload_too_large"), (404, "not_found"), (409, "conflict"),
    (429, "rate_limited"), (503, "unavailable"), (418, "from_a_newer_hub"),
]


def write(folder, prefix, content, seen):
    digest = hashlib.sha256(content).hexdigest()
    if digest in seen:
        return
    seen.add(digest)
    (folder / f"{prefix}-{digest[:12]}").write_bytes(content)


def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()


def main():
    protocol, corpus = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    folder = corpus / "hub"
    folder.mkdir(parents=True, exist_ok=True)
    seen = {hashlib.sha256(p.read_bytes()).hexdigest() for p in folder.iterdir()}

    response = json.loads((protocol / "examples/device/enrol-response.json").read_text("utf-8"))
    write(folder, "enrol", b"\x00" + compact(response), seen)
    write(folder, "enrol", b"\x00" + (protocol / "examples/device/enrol-response.json").read_bytes(),
          seen)
    write(folder, "enrol", b"\x00" + compact({"device_id": "d-1"}), seen)
    write(folder, "enrol", b"\x00" + compact({**response, "later": [1, {"a": None}]}), seen)

    revoked = (protocol / "examples/device/error-revoked.json").read_bytes()
    write(folder, "error", b"\x01" + struct.pack(">H", 401) + revoked, seen)
    for status, code in CODES:
        body = compact({"error": {"code": code, "message": "In plain words."}})
        write(folder, "error", b"\x01" + struct.pack(">H", status) + body, seen)
    write(folder, "error", b"\x01" + struct.pack(">H", 502) + b"<html>Bad gateway</html>", seen)
    write(folder, "error", b"\x01" + struct.pack(">H", 302), seen)

    # The stored form: a format number, then each text with its length (dh_device_encode).
    device_id, token = response["device_id"].encode(), response["device_token"].encode()
    stored = bytes([1, len(device_id)]) + device_id + bytes([len(token)]) + token
    write(folder, "device", b"\x02" + stored, seen)

    write(folder, "secret", b"\x03" + b"q3H7cX0f2pS9L1mB8vN4wA", seen)
    write(folder, "secret", b"\x03" + 'a"b\\c\né\U0001f600'.encode(), seen)

    for name in ("empty-no-pages.json", "demo-three-pages.json"):
        snapshot = json.loads((protocol / "examples/valid" / name).read_text("utf-8"))
        first = compact(snapshot)
        snapshot["generated_at"] = "2030-01-01T00:00:00Z"
        second = compact(snapshot)
        write(folder, "bodies", b"\x04" + struct.pack(">H", len(first)) + first + second, seen)
    write(folder, "bodies", b"\x04" + struct.pack(">H", len(first)) + first + first[:-1], seen)


if __name__ == "__main__":
    main()
