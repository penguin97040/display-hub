"""Writes the seed inputs for fuzz_usb from the USB examples, the example bundles and the
provisioning vectors. Standard library only.

  python3 -I firmware/fuzz/make_usb_corpus.py protocol/v1 firmware/fuzz/corpus

Run it again when they change. It writes only files whose content isn't there already, so running
it with nothing to change changes nothing.

Each input starts with two bytes (see fuzz_usb.c): what the rest is and how the display starts,
then the size of the pieces the port's bytes are fed in. corpus/usb holds the manager's lines as a
stream, each bundle on its own for a display in each starting state, each vector's firmware member
on its own, and stored setups made from the bundles.
"""

import hashlib
import json
import pathlib
import struct
import sys


def write(folder, prefix, content, seen):
    digest = hashlib.sha256(content).hexdigest()
    if digest in seen:
        return
    seen.add(digest)
    (folder / f"{prefix}-{digest[:12]}").write_bytes(content)


def stored_form(bundle):
    """The setup's stored form (dh_setup_encode in firmware/components/dh_core/src/dh_setup.c)."""
    hub = bundle["hub"]
    out = bytes([1, len(hub["hosts"])])
    for host in hub["hosts"]:
        raw = host.encode()
        out += bytes([len(raw)]) + raw
    out += struct.pack("<H", hub["port"]) + bytes([len(hub["trust_anchors"])])
    for anchor in hub["trust_anchors"]:
        pem = anchor["certificate_pem"].encode()
        out += struct.pack("<H", len(pem)) + pem + bytes.fromhex(anchor["spki_sha256"])
    secret = bundle["enrolment"]["secret"].encode()
    out += bytes([len(secret)]) + secret + bundle["enrolment"]["expires_at"].encode()
    wifi = bundle.get("wifi")
    if wifi is None:
        return out + b"\x00"
    ssid, password = wifi["ssid"].encode(), wifi["password"].encode()
    return out + b"\x01" + bytes([len(ssid)]) + ssid + bytes([len(password)]) + password


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    protocol, corpus = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]) / "usb"
    corpus.mkdir(parents=True, exist_ok=True)
    seen = {hashlib.sha256(f.read_bytes()).hexdigest() for f in corpus.iterdir()}
    compact = lambda value: json.dumps(value, separators=(",", ":")).encode()

    examples = protocol / "examples"
    hello = (examples / "usb" / "manager-hello.json").read_bytes().strip()
    provision = json.loads((examples / "usb" / "manager-provision.json").read_text("utf-8"))
    bundles = [provision["bundle"]]
    example = examples / "device" / "provisioning-bundle.json"
    bundles.append(json.loads(example.read_text("utf-8")))

    # The manager's lines as the port delivers them, behind some boot log, in pieces of three
    # sizes, to a new display and to one that is set up.
    stream = b"I (312) boot: ESP-IDF\r\n" + hello + b"\n" + compact(provision) + b"\r\n"
    stream += hello + b"\n"
    for how in (0x00, 0x08, 0x04):
        for piece in (1, 64, 255):
            write(corpus, "stream", bytes([how, piece]) + stream, seen)

    # Each bundle on its own, for a display in each starting state.
    members = []
    for path in sorted((protocol / "firmware").glob("provision-*.json")):
        vector = json.loads(path.read_text("utf-8"))
        member = vector["action"]["bundle_firmware"]
        if member is not None:
            # A key is given by its name in the vector, or written out in full.
            named = [vector["keys"].get(k, k) for k in member["trusted_keys"]]
            member = dict(member, trusted_keys=named)
            members.append(member)
    for bundle in bundles:
        for how in (0x01, 0x05, 0x09, 0x11, 0x21, 0x41, 0x81):
            write(corpus, "bundle", bytes([how, 0]) + compact(bundle), seen)
        for member in members:
            write(corpus, "bundle", bytes([0x01, 0]) + compact(dict(bundle, firmware=member)), seen)
        write(corpus, "stored", bytes([0x02, 0]) + stored_form(bundle), seen)
        write(corpus, "stored", bytes([0x02, 0]) + stored_form(dict(bundle, wifi=None)), seen)

    # Each vector's firmware member on its own, to a display with a key and to one without.
    for member in members:
        for how in (0x03, 0x07):
            write(corpus, "member", bytes([how, 0]) + compact(member), seen)
    return 0


if __name__ == "__main__":
    sys.exit(main())
