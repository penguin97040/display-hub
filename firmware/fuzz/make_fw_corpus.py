"""Writes the seed inputs for fuzz_fw_manifest and fuzz_fw_download from the firmware update
vectors. Standard library only.

  python3 -I firmware/fuzz/make_fw_corpus.py protocol/v1/firmware firmware/fuzz/corpus

Run it again when the vectors change. It writes only files whose content isn't there already, so
running it with nothing to change changes nothing.

corpus/fw_manifest holds each distinct manifest, each distinct header, a few bundle members and
the stored forms of an empty key store and a fresh update record.
corpus/fw_download holds, for each package vector, its body as a hub would send it (two bytes
choosing that, then the body), and for each distinct manifest the same manifest and image for
the target to sign itself (see fuzz_fw_download.c for the layout).
"""

import hashlib
import json
import pathlib
import struct
import sys

HEADER = 137


def write(folder, prefix, content, seen):
    digest = hashlib.sha256(content).hexdigest()
    if digest in seen:
        return
    seen.add(digest)
    (folder / f"{prefix}-{digest[:12]}").write_bytes(content)


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    vectors, corpus = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    manifests, downloads = corpus / "fw_manifest", corpus / "fw_download"
    seen = {}
    for folder in (manifests, downloads):
        folder.mkdir(parents=True, exist_ok=True)
        seen[folder] = {hashlib.sha256(f.read_bytes()).hexdigest() for f in folder.iterdir()}
    signed = set()
    for path in sorted(vectors.glob("*.json")):
        vector = json.loads(path.read_text(encoding="utf-8"))
        if vector["kind"] == "provision":
            member = vector["action"]["bundle_firmware"]
            if member:
                keys = [vector["keys"].get(k, k) for k in member["trusted_keys"]]
                text = json.dumps(
                    {"trusted_keys": keys, "security_counter": member["security_counter"]},
                    separators=(",", ":"),
                )
                write(manifests, "bundle", text.encode(), seen[manifests])
            continue
        if vector["kind"] != "package":
            continue
        body = bytes.fromhex(vector["response"]["body_hex"])
        write(downloads, "body", b"\x00\x61" + body, seen[downloads])
        if len(body) < HEADER:
            continue
        write(manifests, "header", body[:HEADER], seen[manifests])
        (length,) = struct.unpack_from("<H", body, 6)
        manifest = body[HEADER : HEADER + length]
        if not 2 <= length <= 4096 or len(manifest) != length or manifest in signed:
            continue
        signed.add(manifest)
        write(manifests, "manifest", manifest, seen[manifests])
        image = body[HEADER + length :]
        write(
            downloads,
            "signed",
            b"\x01\x61" + struct.pack("<H", length) + manifest + image,
            seen[downloads],
        )
    # The stored forms of an empty key store and a fresh update record (dh_update_state.c).
    write(manifests, "stored", b"\x01" + bytes(200), seen[manifests])
    record = bytearray(321)
    record[0], record[34] = 1, 5
    write(manifests, "stored", bytes(record), seen[manifests])
    for folder in (manifests, downloads):
        print(f"{folder}: {len(list(folder.iterdir()))} files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
