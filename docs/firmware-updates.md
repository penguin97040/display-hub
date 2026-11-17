# Signed firmware updates

**Status: a design, written 17/11/2026 (UTC). Nothing here is built yet.** It is the specification for step 5 of the plan: the first install from the app over USB, then signed updates through the hub. Names, limits and message shapes may still change until the simulator tests exist; after that, this document changes only together with them.

This document is for makers who write display firmware, and for anyone reviewing the design. It uses MUST, SHOULD and MAY in their usual specification sense. Each **judgement call** is marked as one, with the alternative that was turned down.

## The idea in one page

- A maker signs each firmware release with their own private key, on their own computer. The result is one file, a **package** (`.dhfw`).
- A display holds up to three **trusted keys**: the public halves of its maker's keys. They are put there at the first USB install, by the app, from the package that was just flashed.
- An update reaches a display only through the hub, over the same pinned HTTPS connection the display already uses. The hub stores packages and passes them on. **The display decides.** It checks the signature with a key it already holds, checks that the package is for its board, checks that it isn't an old release with known holes, and only then writes anything to flash.
- The hub, the app and the network are therefore not trusted with the question "is this firmware genuine?". A hub that has been taken over can hold updates back, or offer a genuine release at a moment the owner didn't choose, but it cannot make a display run firmware its maker didn't sign.
- If new firmware doesn't start properly and reach the hub within five minutes, the display returns to the previous version by itself.

| Who | Holds | Can do | Cannot do |
|---|---|---|---|
| Maker | The private signing key | Sign a release for their own device type | Reach anyone's display directly |
| Hub | Packages, offers, results. No signing key, ever | Store a package, offer it to one display, serve it, record the result | Make a display accept an unsigned, wrongly signed, wrong-board or downgraded package |
| App | An admin token | Upload a package, show who signed it, press **Update**, do the first USB install | Sign anything; change a display's trusted keys over the network |
| Display | Trusted public keys, a security floor | Verify and install; roll back; report | — |

## Terms

| Term | Meaning |
|---|---|
| Package | One `.dhfw` file: a header, a manifest, the app image, and optionally the extra parts a first USB install needs. |
| Manifest | A small JSON object inside the package that describes the image. It is covered by the signature. |
| Trusted key | An ECDSA P-256 public key a display accepts signatures from. A display holds 0 to 3. |
| Key id | The SHA-256 of a public key's 65 bytes, as 64 lower-case hex digits. |
| Fingerprint | The first 32 hex digits of the key id, shown in groups of four (`3f9a-01c2-…`). This is what people compare. |
| Security counter | A whole number in every manifest. A maker raises it when a release fixes a security hole, or retires a key. |
| Security floor | The highest security counter a display has confirmed. It refuses any package below it. |
| Offer | The hub telling one display "install this package". Made when the owner presses **Update**. |
| Trial boot | The first start of newly installed firmware, before it has proved itself. |

## Cryptography

| Choice | Value |
|---|---|
| Signature | ECDSA on NIST P-256 (secp256r1), over a SHA-256 digest |
| Signature encoding | Fixed 64 bytes: `r` then `s`, each 32 bytes, big-endian, zero-padded on the left. Not DER. |
| Public key encoding | 65 bytes: `0x04`, then X, then Y (uncompressed SEC 1). Written as 130 lower-case hex digits in JSON. |
| Hash | SHA-256 everywhere: the signed digest, the image hash, key ids and package ids |

**Why not Ed25519 (judgement call).** Ed25519 would be the tidier choice, but Mbed TLS 3.6, which ESP-IDF 5.x ships, does not implement EdDSA: its PSA configuration lists ECDSA, deterministic ECDSA and RSA signatures and no Edwards curve, and the Mbed TLS roadmap still lists EdDSA as future work. Using it would mean adding a second cryptography library to every display's firmware. ECDSA P-256 with SHA-256 is already in every display, because the hub's own TLS certificate uses it and `tools/mbedtls-interop` tests it. Chips with an ECC accelerator use it; the rest verify in software, which takes a fraction of a second, once per update.

**How each side does it.**

- *Signing (Rust):* `ring` 0.17, already pinned in this workspace for TLS: `EcdsaKeyPair` with `ECDSA_P256_SHA256_FIXED_SIGNING`. `ring` signs with a random nonce from the operating system, so signing the same package twice gives two different, equally valid signatures.
- *Verifying (Rust, in the hub, app, tool and simulator):* `ring::signature::ECDSA_P256_SHA256_FIXED`.
- *Verifying (firmware):* either `psa_verify_hash` with `PSA_ALG_ECDSA(PSA_ALG_SHA_256)`, which takes exactly the 64-byte form above and a public key imported in the 65-byte form, or the older `mbedtls_ecdsa_verify` with `r` and `s` read as two big-endian numbers. The PSA call is the one to prefer in new firmware, because later Mbed TLS releases are built around PSA.

A display MUST NOT accept any other algorithm, and there is no "unsigned" or "development" mode in the format.

## The package

### Layout

All integers in the header are little-endian. Offsets are in bytes.

| Offset | Length | Field | Rule |
|---|---|---|---|
| 0 | 4 | Magic | The ASCII bytes `DHFW` |
| 4 | 1 | Format version | `1` |
| 5 | 1 | Algorithm | `1` (ECDSA P-256 with SHA-256, as above) |
| 6 | 2 | `manifest_len` | 2 to 4096 |
| 8 | 65 | Signer's public key | First byte `0x04` |
| 73 | 64 | Signature | `r` then `s` |
| 137 | `manifest_len` | Manifest | UTF-8 JSON, one object |
| 137 + `manifest_len` | `image_size` | App image | Exactly as built (the ESP-IDF application `.bin`) |
| after the image | sum of part sizes | USB-install parts | In the order the manifest lists them. Absent if the manifest has no `usb_install`. |

- The file's length MUST equal the sum of those lengths exactly. Trailing bytes make the package invalid.
- The **update form** of a package is its first `137 + manifest_len + image_size` bytes: everything a display needs, and nothing a display doesn't. This is what the hub serves to displays. USB-install parts are never sent over Wi-Fi.
- The **package id** is the SHA-256 of the whole file, as 64 lower-case hex digits. It is only a name for the file. It is not a security check: the signature is.
- The recommended file name is `<device_type>-<firmware_version>.dhfw`. Nothing reads the name.

### The signed bytes

The signature is over the SHA-256 of these bytes, joined in this order:

1. the 23 ASCII bytes `display-hub.firmware.v1`, then one zero byte (24 bytes in all);
2. bytes 0 to 72 of the header (magic, format version, algorithm, `manifest_len` and the signer's public key: 73 bytes);
3. the manifest, exactly as it appears in the file (`manifest_len` bytes).

The image and the USB-install parts are bound through the manifest, which carries the length and SHA-256 of each. The fixed text at the front means a signature made for anything else can never be mistaken for a firmware signature. The signer's key is inside the signed bytes, so the key id is bound too.

The manifest is signed **as bytes**, and verified as the same bytes before anything parses it. No one ever re-serialises JSON to check a signature, so there is no "canonical JSON" to get wrong.

### The manifest

```json
{
  "device_type": "llm-usage-display",
  "hardware_revisions": ["waveshare-amoled-1.8-v1", "waveshare-amoled-1.8-v2"],
  "firmware_version": "1.2.0",
  "security_counter": 3,
  "image_size": 1482752,
  "image_sha256": "<64 hex digits>",
  "summary": "Fixes the clock drifting after a long outage.",
  "key_set": ["<130 hex digits>", "<130 hex digits>"],
  "usb_install": {
    "chip": "esp32s3",
    "min_flash_bytes": 16777216,
    "app_offset": 65536,
    "parts": [
      { "name": "bootloader", "offset": 0, "size": 21072, "sha256": "<64 hex digits>" },
      { "name": "partition-table", "offset": 32768, "size": 3072, "sha256": "<64 hex digits>" }
    ]
  }
}
```

| Field | Required | Rule |
|---|---|---|
| `device_type` | Yes | 1 to 48 characters of plain text, the same rule as the `device_type` a display reports. Compared byte for byte. |
| `hardware_revisions` | Yes | 1 to 8 strings, each 1 to 48 characters, no repeats. Compared byte for byte with the display's `hardware_revision`. No wildcards. |
| `firmware_version` | Yes | `MAJOR.MINOR.PATCH`, optionally followed by `-` and a pre-release tag of letters, digits, `.` and `-` (`1.2.0`, `1.3.0-beta.1`). At most 48 characters. |
| `security_counter` | Yes | A whole number from 0 to 4294967295. |
| `image_size` | Yes | A whole number from 1 to 8388608 (8 MiB). |
| `image_sha256` | Yes | 64 lower-case hex digits: the SHA-256 of the app image. |
| `summary` | No | Plain text for the app to show, at most 280 characters. |
| `key_set` | No | 1 to 3 public keys, no repeats: the complete set of keys displays should trust from this release on (see [Changing keys](#changing-a-displays-trusted-keys)). |
| `usb_install` | No | What the app needs for a first install over USB. A display ignores it. |
| `usb_install.chip` | Yes, inside it | The chip the image is built for, as a lower-case name: `esp32`, `esp32s2`, `esp32s3`, `esp32c3`, `esp32c6` and so on. |
| `usb_install.min_flash_bytes` | Yes, inside it | The smallest flash chip the layout fits. |
| `usb_install.app_offset` | Yes, inside it | Where the app image is written in flash. A multiple of 4096. |
| `usb_install.parts` | Yes, inside it | 1 to 4 parts, each with a `name` (lower-case letters, digits and `-`, up to 32 characters), an `offset` (a multiple of 4096), a `size` (1 or more) and a `sha256`. |

Rules for the JSON itself:

- one object, UTF-8, no byte-order mark, at most 4096 bytes, nested no deeper than four levels;
- no member name appears twice in an object;
- numbers are whole, written without a sign, fraction or exponent;
- a field that isn't listed above is ignored, so the format can grow. A change that an older display must not ignore gets a new format version in the header instead.

### Limits

| Limit | Value |
|---|---|
| Header | 137 bytes |
| Manifest | 4096 bytes |
| App image | 8 MiB |
| Whole package | 16 MiB |
| Hardware revisions in a manifest | 8 |
| Trusted keys on a display, and keys in a `key_set` | 3 |
| USB-install parts | 4 |

**Judgement call: JSON for the manifest.** A fixed binary layout would be smaller to parse. JSON was chosen because makers and reviewers can read it, every display already has a bounded JSON parser for snapshots, and a display parses the manifest only *after* the signature over its exact bytes has verified, so the display's parser never meets a manifest that a trusted key didn't sign. The hub, the app and the tool do parse manifests from anyone, in Rust, with every bound above checked.

### Checking a package (hub, app and tool)

Anything that isn't a display checks a package in this order, reading it as a stream and never holding more than the header and manifest in memory:

1. The length is at least 139 bytes and at most 16 MiB.
2. The header: magic, format version, algorithm, `manifest_len`, and a public key that is a valid P-256 point.
3. The signature verifies with the public key in the header.
4. The manifest parses and every rule above holds. Unlike a display, these checkers refuse a repeated member name outright.
5. With `usb_install`: `app_offset` and every part lie inside `min_flash_bytes`, and no two regions overlap (the app image counts as a region).
6. The file length is exactly header + manifest + image + parts.
7. The SHA-256 of the image, and of each part, matches the manifest.

Passing these checks means the package is **well formed and consistent with the key inside it**. It says nothing about whether that key is one anyone should trust. Only a display's own stored keys answer that.

## What a display must do

### What it stores

| Item | Notes |
|---|---|
| Trusted keys (0 to 3 public keys) | Public, so they need no secrecy. Kept in an NVS namespace of their own. |
| Security floor | One number. Kept with the keys. |
| Update record | The last offer id acted on, its package's version and security counter, any `key_set` waiting to be applied, and the last result. Small. |

A **factory reset** on the device erases the hub details, Wi-Fi settings and token, as today. It MUST NOT erase the trusted keys or the security floor. Those belong to the firmware's maker, not to the hub the display happens to be paired with.

### Checks before installing

A display acts on an offer (see [Protocol additions](#protocol-additions)) by requesting the update form and checking it as it arrives. The order matters: every check before step 8 happens before a single byte is written to flash.

1. **No keys, no updates.** A display with no trusted keys MUST refuse every offer (`no_trusted_keys`).
2. **Size first.** The offer's `size`, and the response's `Content-Length`, MUST be equal and no more than 137 + 4096 + the size of the display's spare app partition. Otherwise refuse (`too_large`) without reading the body.
3. **Header.** Read 137 bytes. The magic, format version (1), algorithm (1) and `manifest_len` (2 to 4096) MUST be right (`bad_format`).
4. **Known key.** The public key in the header MUST equal one of the display's trusted keys, all 65 bytes (`untrusted_key`). The display MUST verify with **its stored copy**. It MUST NOT trust a key because a package, the hub or the app presents it.
5. **Signature.** Read the manifest, and verify the signature over the signed bytes (`bad_signature`). The display MUST NOT parse the manifest before this passes.
6. **Manifest.** Parse it. A missing or mistyped required field, or a value outside its range, is `bad_format`.
7. **Is it for this display, and not a downgrade?**
   - `device_type` MUST equal the display's own (`wrong_device_type`).
   - `hardware_revisions` MUST contain the display's own (`wrong_hardware`).
   - `security_counter` MUST be at least the display's security floor (`downgrade`).
   - `image_size` MUST fit the spare app partition, and `137 + manifest_len + image_size` MUST equal the `Content-Length` (`too_large`, `bad_format`).
8. **Download and hash.** Only now open the spare partition (`esp_ota_begin`) and write the image as it arrives, hashing it. Read exactly `image_size` bytes and no more. The SHA-256 MUST equal `image_sha256`, or the display abandons the partition (`esp_ota_abort`) and reports `image_hash`.
9. **Finish.** `esp_ota_end` checks that the image is a valid app image (`flash`, if it or any write fails). Save the update record, set the new partition as the one to boot (`esp_ota_set_boot_partition`) and restart.

A display MUST use the same pinned TLS settings for the download as for everything else, and MUST send its token. Because the update form has a header in front of the image, firmware uses `esp_http_client` with the `esp_ota_*` functions directly, not `esp_https_ota`, which expects a bare image.

### Limits on trying

- A display MUST act on a given `offer_id` at most once: one attempt per offer. It writes the offer id to its update record before it requests the download, so it still knows after a restart or a power cut. Trying again takes a new offer (the owner presses **Update** again).
- It MUST leave at least 10 minutes between download attempts, and after three failures in a row, at least 6 hours (or until it is next restarted by its owner). Every attempt can erase and write a partition, and flash wears out; these limits stop a faulty or hostile hub wearing it down.
- It MUST give up on a download that sends nothing for 30 seconds, or takes more than 15 minutes in all (`download`). Downloads are not resumed: the next attempt starts again from the beginning.
- It SHOULD show that it is updating, and SHOULD keep showing its pages until the restart.

### The trial boot, and when new firmware counts as good

Firmware MUST be built with ESP-IDF's app rollback (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`) and a partition table with two app partitions and an OTA data partition. The bootloader then gives a new image **one** start to prove itself: if the display resets for any reason before the image is marked good, the bootloader goes back to the previous image on the next start.

New firmware, on its trial boot (`esp_ota_get_state_partition` reports `ESP_OTA_IMG_PENDING_VERIFY`):

1. MUST start its screen and storage, join Wi-Fi, and make an authenticated request to the hub over pinned TLS.
2. MUST mark itself good (`esp_ota_mark_app_valid_cancel_rollback`) as soon as the hub answers that request with success, or with an error from the "set up again" group (`token_revoked`, `unknown_device`): rolling back wouldn't fix those.
3. MUST otherwise give up after **5 minutes** and roll back (`esp_ota_mark_app_invalid_rollback_and_reboot`).
4. Once marked good, and only then, MUST raise the security floor to the new release's `security_counter` if that is higher, replace the trusted keys with the release's `key_set` if it had one, clear the update record's pending parts, and report `installed`.

The previous firmware, when it finds itself running again with an update record it didn't finish, MUST drop the pending `key_set`, leave the floor alone, and report `rolled_back`.

**Judgement call: "good" means "reached the hub".** ESP-IDF only asks for a self-test. This design asks for a real check-in, because firmware that can't reach the hub can never be updated again, and that is exactly the failure a display can't recover from without a cable. The cost is that an update fails, harmlessly, if the hub or Wi-Fi is down for those five minutes, or the power is cut during them: the display rolls back and the owner presses **Update** again. Five minutes is short on purpose: ESP-IDF advises finishing the self-test quickly, since a power cut before it finishes causes a rollback.

### Downgrades and replays

- The **security counter** decides, not the version text. A display accepts any correctly signed package for its board whose counter is at or above its floor. An older *version* with the same counter is allowed, so an owner can go back to the last release after a bad one. The app asks for confirmation first.
- A maker MUST raise the counter in any release that fixes a security hole, and in any release whose `key_set` removes a key. Once a display has confirmed that release, every earlier one is refused for good, however it is delivered.
- The floor is kept in ordinary flash (NVS), not in eFuses. See the next section for why.

### Secure Boot, flash encryption and eFuse anti-rollback

**Recommendation: don't turn these on for the standard firmware, and don't require them.** This design does not depend on any of them.

| Feature | What it would add | Why it stays off by default |
|---|---|---|
| Secure Boot v2 | The chip itself refuses a bootloader or app not signed with a key whose digest is burnt into eFuses. Stops someone with a cable flashing other firmware. | Burning eFuses can't be undone. A lost key means that board can never be updated. It needs ESP32 chip revision 3.0 or later, signs with RSA-3072 on the ESP32 and ESP32-S3 (a second key, of another kind, to look after), and turns off the USB-OTG updater in the chip's ROM. A mistake leaves a board that can't be recovered, which is the wrong risk for people building their first display. |
| Flash encryption | Keeps the flash contents (Wi-Fi password, token) from someone who reads the chip. | Also eFuses. In release mode a board can no longer be flashed with ordinary firmware over USB, and flashing unencrypted data to an encrypted board stops it starting. By itself it doesn't stop firmware being altered. |
| eFuse anti-rollback (`secure_version`) | A downgrade floor that survives erasing the flash. | Each step burns an eFuse bit for good: 32 steps on the ESP32 and 16 on the ESP32-S3. It rules out a factory partition. Without Secure Boot it adds little, because someone with a cable can replace the bootloader that checks it. |

What this means, plainly: **someone holding a display and a USB cable can put any firmware on it.** That has always been true of hobby ESP32 boards, and signed updates don't change it. Signed updates protect against everything that arrives over the network, including through the hub. A maker who wants protection against a cable as well MAY enable Secure Boot v2 and flash encryption on their own boards; packages then carry an image that is also signed for Secure Boot, and the first install is the maker's job, not the app's.

## Keys

### For makers: making and keeping a key

The signing tool, `hub-firmware`, makes keys:

```sh
hub-firmware keygen --out my-displays.dhkey
```

- The key is created on the maker's computer from the operating system's random source. It never leaves that computer, and it is never sent to a hub, an app or a display.
- **The key file is always protected by a passphrase**, asked for on the terminal and never taken from the command line. The file holds the public key in the clear, and the private key (PKCS#8) encrypted with AES-256-GCM under a key derived from the passphrase with PBKDF2-HMAC-SHA256 (600,000 rounds, a random 16-byte salt). The tool refuses a passphrase shorter than 12 characters. The file is created readable by its owner only.
- `keygen` prints the fingerprint. **Publish the fingerprint** where your users will see it (your README, your release page): it is how they check a first install.
- **Make two keys.** One is for everyday signing. The other is a **recovery key**: make it, note its fingerprint, copy its file to two offline places (a USB stick in a drawer, a password manager), and don't use it. Name both in `key_set` from your very first release, so every display trusts both from its first install.
- A key file is never committed to a repository, and a CI system should not hold one unless the maker accepts that whoever controls the CI can sign.

**Judgement call: a passphrase-protected file, not the OS keyring.** One file behaves the same on Windows, macOS and Linux, can be backed up by copying it, and can be used on a build machine with no desktop. A key that lives only in a keyring is lost with the computer. Keyring support for remembering the passphrase may come later.

**Judgement call: PBKDF2, not a memory-hard function.** PBKDF2 and AES-GCM are already in `ring`, so the signing tool adds no new cryptography code. Argon2id would slow passphrase guessing more, at the price of another dependency. The passphrase is the second line of defence for a file that should never leave the maker's computer.

### If a key is lost or leaked

| What happened | What to do | What it costs |
|---|---|---|
| The signing key is **lost**, the recovery key is safe | Make a new signing key. Sign the next release with the recovery key, with a `key_set` naming the new key and the recovery key (or a new recovery key), and a higher security counter. | Nothing for owners. They update as usual. |
| The signing key is **leaked** | The same, at once. Tell your users. | Displays that install the release stop trusting the leaked key. Until then, the leaked key is only useful to someone who can also get a package to the display: through the owner's hub or app, or with a cable. |
| **Both** keys are lost | Make new keys and publish the new fingerprints. | Every owner reinstalls over USB once, from the app. |
| Both keys are leaked | New keys, new fingerprints, and tell your users. | Every owner reinstalls over USB. A display that was given hostile firmware in the meantime can only be trusted again after that. |

### Changing a display's trusted keys

There are exactly two ways.

1. **A signed release with a `key_set`.** The release is signed by a key the display already trusts. After the release has been installed and marked good, the display replaces its whole set of trusted keys with the `key_set`. A `key_set` may leave out the key that signed it, which is how a key is retired.
2. **A first install over USB**, which erases the display and starts again (next section).

Nothing else changes them: not the hub, not the app over the network, not a factory reset, not a provisioning bundle sent to a display that already has keys.

**Judgement call: key changes ride inside firmware releases.** There is no separate "key change statement". It would be one more signed format to parse, and it would add no safety: firmware runs with full control of the display, so whoever can sign firmware a display accepts can already change its keys. Carrying the new set in the manifest makes that honest and keeps it tied to a release that has proved it starts.

## The first install, and what a cable may do

### Install firmware, in the app

1. The owner plugs the display in and chooses **Install firmware**, then picks a package (a file from the maker, or the official one).
2. The app checks the package as [above](#checking-a-package-hub-app-and-tool). It needs `usb_install`.
3. **The app shows who signed it, before writing anything:**
   - *Official Display Hub firmware*, when the key is one built into the app itself; or
   - *Signed by a maker this app doesn't know*, with the fingerprint, and the instruction to compare it with the fingerprint the maker publishes. The owner must confirm.

   The name comes from the app or from the owner, never from the package: a package can't name itself.
4. The app connects to the chip's built-in loader, checks the chip is the one in `usb_install.chip` and that the flash is at least `min_flash_bytes`, **erases the whole flash**, and writes each part and the app image at its offset, as raw bytes.
5. The display restarts. The app sends the provisioning bundle, which now carries the trusted keys: the package's `key_set` if it has one, otherwise the one key that signed it. It also carries the package's security counter as the starting floor.
6. The display enrols with the hub as usual.

**Who chooses the key?** The package the owner chose, through the app. The hub has no part in it: the bundle's firmware section is built by the app from the package, and the hub never sees or supplies it. The owner's confirmation in step 3 is the one moment of trust in the whole design ("trust on first install"), which is why the app makes the fingerprint hard to miss.

### What each action may change

| Action | Hub details, Wi-Fi, token | Trusted keys | Security floor |
|---|---|---|---|
| **Install firmware** from the app (USB, erases everything) | Erased, then set by the bundle | Erased, then set from the package | Erased, then set from the package |
| **Factory reset** on the device | Erased | Kept | Kept |
| **Provisioning bundle** over USB to a reset display that has keys | Set | Unchanged. A bundle naming a different set of keys is refused (`firmware_keys_locked`). A bundle naming the same set, or none, is accepted. | Unchanged |
| **Provisioning bundle** to a display with no keys | Set | Set from the bundle, if it has any | Set from the bundle |
| A signed update over Wi-Fi | Unchanged | Replaced by its `key_set`, if it has one, once marked good | Raised, once marked good |

So a display that is already set up can be given new keys only by wiping it with **Install firmware**, and a factory reset followed by a cable can point a display at another hub (as today) but can't change whose firmware it accepts. Someone with a cable can always wipe the display and install their own firmware; that is physical access, and it is outside what this design defends (see [Secure Boot](#secure-boot-flash-encryption-and-efuse-anti-rollback)).

A display with no trusted keys (set up by an older app, or from a bundle without them) works normally and refuses every update. The app says so, and offers **Install firmware**.

## The hub's part

The hub stores, offers and serves. It holds no signing key, never signs, never alters a package, and never fetches one from the internet: packages arrive only by upload from the local network.

### Storing packages

- An upload is streamed to a temporary file in the hub's data folder, never held in memory, and checked as [above](#checking-a-package-hub-app-and-tool), signature included. A package that fails is deleted and refused with the reason. This is defence in depth, and catches damaged files early; **a display never relies on it.**
- A stored package is a file named by its package id, readable only by the service, plus one row describing it. Uploading the same file twice is harmless.
- **Limits:** at most 3 packages for each `device_type` and 256 MiB of packages in all. When a fourth arrives for a device type, the hub removes that type's lowest version that isn't on offer to any display. An upload that would pass 256 MiB, or leave less than 64 MiB free on the disk, is refused.
- Packages are not included in `hubd backup`: they are large, and can be uploaded again. After a restore, the hub forgets any package whose file is missing.

### Who may upload

- **An admin** (the app).
- **A publisher**: a new, narrow kind of pairing for the maker's `hub-firmware` tool, invited from the app or with `hubd pair-publisher`, in the same way as a helper. A publisher token (`dhp_…`) can upload packages and list them. It cannot make an offer, delete a package another uploader added, or do anything else.

**Judgement call: a publisher role instead of giving the tool an admin token.** It means one more token type, but a maker's build machine then holds a token that can only add a package to the shelf. Installing still takes the owner pressing **Update**, and the display's own checks.

### Offering an update to one display

- An offer is made only by an admin, for one display and one package: the **Update** button.
- The hub refuses to make an offer (`conflict`, with a plain message) when, by the display's own last report, the package's `device_type` or hardware revisions don't match, the display doesn't trust the signer, the package is below the display's security floor, or the image is too big for its spare partition. These checks save a pointless download and give the owner a clear message. They are a courtesy: the display repeats every one of them.
- A display has at most one open offer. An offer ends when the display reports a final result, when an admin cancels it, when the display is removed, or **24 hours** after it was made, so a display that has been in a drawer doesn't update weeks later on a forgotten press.
- **Room for "install automatically":** each display has an `auto_update` setting, stored from the start and always off for now. When it is built, the hub will make the offer itself when a newer package arrives from a signer the display already trusts. Nothing changes for displays: an automatic offer looks the same and passes the same checks.

### Serving the download

`GET /v1/device/firmware/{package_id}` (below) is the only way a package leaves the hub towards a display.

- Pinned TLS and the display's token, like every device request. The hub serves a package only to a display that has an open offer for it; anything else is `not_found`.
- The answer is the update form, with an exact `Content-Length`, no compression and no chunked encoding.
- **Not resumable (judgement call).** Range requests are ignored. An image is a few megabytes on a local network, an interrupted download simply starts again, and resuming would add state and parsing to both ends.
- At most 2 downloads at once across the hub (`unavailable` otherwise), and at most 6 download starts per display per hour (`rate_limited`). The request has its own 15-minute time limit, in place of the hub's usual 10 seconds.

### Results

The hub records what each display reports about an offer and shows it to admins. A report is a claim by the display, authenticated by its token. The hub also notices a display's next heartbeat naming the offered version, and counts that as installed if the report itself was lost. Offers, results, uploads and deletions go in the audit log.

## The app's part

- **Upload:** choose a package file, or drop it on the window. The app checks it first, then sends it to the hub.
- **Who signed it:** every package and every display shows a signer as a name and a fingerprint. The name is *Official Display Hub firmware* for keys built into the app, or the label the owner gave that key at a first install, or *Unknown maker*. Labels are kept on the hub for convenience, so they are only as trustworthy as the hub; the fingerprint is what counts, and the display's own keys are what decide.
- **Update available:** shown for a display when the hub holds a package for its device type and hardware revision, signed by a key the display reports trusting, with a higher version. Versions are compared as three numbers; with the numbers equal, a version with a pre-release tag is older than one without.
- **Update:** the button names the version, the signer and the summary. Going to an older version, or to a pre-release, asks for a second confirmation and says why.
- **Progress and results:** *Waiting for the display to check in* (it acts at its next poll, within five minutes by default), *Downloading*, *Restarting*, then *Updated to 1.2.0*; or a plain sentence for each failure. A rollback reads: *The update didn't start properly, so the display went back to 1.1.0. Nothing was lost.*
- **Install firmware:** the first install over USB, as [above](#install-firmware-in-the-app).

The app never holds a signing key. Signing is done with `hub-firmware`, by whoever makes the firmware.

## The signing tool

`hub-firmware` is one command-line program for makers, on Windows, macOS and Linux.

| Command | Does |
|---|---|
| `hub-firmware keygen --out <file>` | Makes a key file and prints its fingerprint. |
| `hub-firmware sign --key <file> --image app.bin --device-type … --hardware … --version … --security-counter … [--key-set <public key file> …] [--usb-part name@offset=file … --app-offset … --chip … --min-flash …] --out <package>` | Builds and signs a package, then checks its own output. |
| `hub-firmware inspect <package>` | Checks a package and prints its manifest, signer fingerprint and package id. Needs no key. |
| `hub-firmware login` | Pairs with a hub as a publisher: paste the publisher string when asked. The token goes in the OS credential store. |
| `hub-firmware add <package>` | Uploads a package to the paired hub. |
| `hub-firmware publish …` | `sign`, then `add`, in one step: the "simple command". |

`sign` SHOULD also warn when the image doesn't look like an ESP-IDF app image, or when the version in the image's own description block (`esp_app_desc_t`, at a fixed offset near the start of the image) differs from `--version`. Official firmware is signed with the same tool and the same format. Nothing about it is special except that its public keys are built into the app, so the app can name it.

## Protocol additions

All of these are additions to protocol v1. Older displays ignore the new fields, and an older hub simply never sends them.

### What a display reports

`device` objects (in enrolment, in every heartbeat and in the USB `hello`) gain one optional member:

```json
"firmware": {
  "key_ids": ["<64 hex digits>", "<64 hex digits>"],
  "security_floor": 3,
  "slot_bytes": 3145728
}
```

`key_ids` are the display's trusted keys (0 to 3), `security_floor` its floor, and `slot_bytes` the size of its spare app partition. The hub stores them with the other device fields and uses them only for the courtesy checks and for what the app shows.

### The offer, in the snapshot

`GET /v1/device/snapshot` gains one optional member, present only while this display has an open offer:

```json
"firmware_update": {
  "offer_id": "<32 hex digits>",
  "package_id": "<64 hex digits>",
  "size": 1484937
}
```

`size` is the length of the update form. A display that doesn't know the member ignores it, as with any unknown field.

### `GET /v1/device/firmware/{package_id}`

Authenticated. `200` with `Content-Type: application/octet-stream`, an exact `Content-Length`, and the update form as the body. Errors use the usual JSON body: `not_found` (no open offer for this display and package), `rate_limited`, `unavailable`.

### `POST /v1/device/firmware/report`

Authenticated; the answer is `204`. A display sends one when it starts downloading and one when the offer is settled, and repeats a final report until it gets an answer.

```json
{ "offer_id": "<32 hex digits>", "state": "refused", "reason": "downgrade", "firmware_version": "1.1.0" }
```

| `state` | Meaning | `reason` |
|---|---|---|
| `downloading` | The manifest passed every check and the image is arriving. | — |
| `installed` | The new firmware started, reached the hub and was marked good. | — |
| `refused` | The display would not install it. Nothing was written. | `no_trusted_keys`, `too_large`, `bad_format`, `untrusted_key`, `bad_signature`, `wrong_device_type`, `wrong_hardware`, `downgrade` |
| `failed` | Something went wrong after the checks. The old firmware is still running. | `download`, `image_hash`, `flash` |
| `rolled_back` | The new firmware didn't prove itself, and the display went back. | — |

`firmware_version` is what the display is running when it sends the report. The hub treats a state or reason it doesn't know as a failure, and shows it as such.

### The provisioning bundle and USB

The bundle gains one optional member:

```json
"firmware": {
  "trusted_keys": ["<130 hex digits>", "<130 hex digits>"],
  "security_counter": 3
}
```

`trusted_keys` holds 1 to 3 public keys, with no repeats. A display follows the table in [What each action may change](#what-each-action-may-change). The USB exchange gains one error code, `firmware_keys_locked`: the display already trusts a different set of keys, and only **Install firmware** can change them. An older manager reads it as an unrecognised error and shows the message.

### Admin and publisher routes

| Route | Who | Does |
|---|---|---|
| `POST /v1/admin/firmware` | Admin | Uploads a package: the raw file as the body, with a `Content-Length` of at most 16 MiB. `201` with the package's description; `200` if the hub already had it. |
| `GET /v1/admin/firmware` | Admin | Every package: id, device type, hardware revisions, version, security counter, summary, signer key id and label, size, whether it can be used for a USB install, when and by whom it was uploaded, and which displays it is on offer to. |
| `GET /v1/admin/firmware/{package_id}/file` | Admin | The whole package, so the app can do a USB install from a package the hub holds. |
| `DELETE /v1/admin/firmware/{package_id}` | Admin | Removes a package and cancels its open offers. |
| `PUT /v1/admin/devices/{id}/firmware-update` | Admin | `{"package_id": …}`: makes the offer (the **Update** button). `DELETE` cancels it. |
| `GET /v1/admin/devices` | Admin | Each display gains `firmware`: what it reported, its open or last offer with its state, reason and times, the best package available for it, and `auto_update`. |
| `GET /v1/admin/firmware/signers`, `PUT`, `DELETE …/signers/{key_id}` | Admin | The owner's labels for signing keys. |
| `GET`, `POST /v1/admin/publishers`, `DELETE /v1/admin/publishers/{id}` | Admin | Publishers: list, invite, revoke. |
| `POST /v1/publisher/pair` | — | Exchanges a publisher string's secret for a publisher token. |
| `POST`, `GET /v1/publisher/firmware` | Publisher | Upload and list, as the admin routes. |

## Threat model

| Threat | What the attacker can do | What stops them, and what is left |
|---|---|---|
| **A compromised hub** (root on the Pi, or a stolen hub key) | Offer anything to any display at any time; serve any bytes; lie to the app about results. | A display installs only what verifies against its own stored keys, for its own board, at or above its floor. **Left over:** the hub can hold updates back, offer a genuine release the owner didn't ask for (including an older one with the same security counter), and force a rollback by not answering a trial boot. It cannot install anything the maker didn't sign. |
| **A compromised PC or app** (a stolen admin token, or malware on the PC) | Upload packages, press Update, relabel signers. | The same display checks. **Left over:** everything above, plus a hostile app can lie on screen. And the app is trusted at a **first USB install**: it writes the firmware and chooses the keys, so a hostile app at that moment owns that display. Downloads of the app are not code-signed, so owners should get it from the project's own releases page. |
| **A stolen publisher token** | Upload well-formed packages to the hub. | Nothing installs without the owner's Update and the display's checks. Revoke the publisher. |
| **A stolen display** | Read its flash: Wi-Fi password, hub details, token, public keys. Flash anything onto it. | There is no signing secret on a display. Revoke it in the app, as today. Reflashing by cable is not defended (optional Secure Boot, above). |
| **A leaked maker key** | Sign firmware that displays trusting that key accept. | The attacker must still deliver it: through a compromised hub or app on that household's network, or by tricking the owner into uploading a package and pressing Update. The maker's next release retires the key and raises the counter; displays that install it are safe from then on. A recovery key means this needs no cable. |
| **A network attacker** (same Wi-Fi, spoofed names or addresses) | Intercept, replay or alter traffic. | Pinned TLS, as for all display traffic: nothing reaches the display. Even without TLS, the signature and image hash would catch any change. |
| **A malicious package** (aimed at a parser) | Send crafted bytes to the hub, app, tool or display. | Fixed-size header, hard length limits checked before reading, exact total length, streaming with nothing large held in memory. Hub, app and tool parse in Rust with every bound checked, and never interpret the image. A display parses a manifest only after its signature verified against a trusted key, and writes to flash only after every manifest check. The app writes USB parts as raw bytes at checked, non-overlapping offsets. |
| **An interrupted download or a power cut** | — | The image goes to the spare partition; the running firmware is untouched until the image is complete and its hash matches. A power cut while writing leaves the old firmware booting. A power cut during the trial boot rolls back. The next attempt starts from the beginning. |
| **A wrong-board image** | Offer firmware built for another display. | The signed manifest names the device type and hardware revisions, and the display compares both with its own, exactly. The hub and app check too, earlier. At a USB install the app checks the chip. ESP-IDF also validates the app image before accepting it. If a wrong image still got through, it would fail its trial boot and roll back. |
| **Replaying an old signed image** | Offer a genuine but older release with a known hole. | The security floor: once a display has confirmed a release with a higher counter, older ones are refused. **Left over:** releases with the *same* counter can be swapped freely, so makers must raise it for every security fix. A full USB wipe resets the floor (physical access). |
| **A hostile hub wearing out flash** | Offer again and again. | One attempt per offer, at least 10 minutes between attempts, 6 hours after three failures, and no flash write before the signature and manifest checks pass. |

## What the simulator must test

`tools/display-simulator` is the reference display. It will keep two "partitions" as files, a trusted-key store, a floor and an update record in its state folder, and follow every MUST above. The cases below become language-neutral vectors under `protocol/v1/firmware/` (a package, the display's state before, and the expected outcome), so firmware can port them to unit tests. Test packages are signed with throwaway keys made for the purpose; their private halves are not kept or published.

**Package checks (display).** Each ends with the old firmware running and nothing written unless stated.

1. A valid package for this display: installed; floor and version updated.
2. Wrong magic. → `bad_format`
3. Format version 2. → `bad_format`
4. Algorithm 2. → `bad_format`
5. `manifest_len` of 0, 1 and 4097. → `bad_format`
6. A body shorter than the header. → `download`
7. Signed by a key the display doesn't hold, otherwise perfect. → `untrusted_key`
8. A trusted key in the header, with one bit of the signature flipped. → `bad_signature`
9. A trusted key, with one byte of the manifest changed after signing. → `bad_signature`
10. A trusted key, with a signature of all zeros. → `bad_signature`
11. A valid signature made with the right key over the same bytes **without** the fixed text in front. → `bad_signature`
12. A display with no trusted keys. → `no_trusted_keys`
13. Signed by the display's second trusted key: installed.
14. Manifest that isn't JSON; one missing each required field in turn; a number written as a string; a negative or fractional `image_size`. → `bad_format`
15. A manifest with unknown extra fields: installed.
16. `device_type` differing by one character, and by letter case. → `wrong_device_type`
17. `hardware_revisions` without this display's revision; and with it as a prefix of a longer string. → `wrong_hardware`
18. `security_counter` one below the floor. → `downgrade`. Equal to the floor, with an older version: installed.
19. `image_size` larger than the spare partition; offer `size` larger than the limit. → `too_large`, with no body read.
20. `Content-Length` that disagrees with the offer's `size`, or with the manifest's lengths. → `too_large` or `bad_format`
21. The image with one byte changed. → `image_hash`; the spare partition is abandoned.
22. A body that ends early; one that stalls. → `download`
23. A body with extra bytes after the image: the display reads `image_size` bytes and no more.

**Offers and limits.**

24. The same `offer_id` seen again after a refusal: nothing happens.
25. A new offer inside 10 minutes of the last attempt: it waits.
26. Three failures in a row: the next attempt waits 6 hours.
27. A download for a package that isn't on offer to this display: `not_found` from the hub.
28. An offer older than 24 hours: gone from the snapshot.

**Trial boot and rollback.**

29. New firmware reaches the hub: marked good, reports `installed`, floor raised.
30. New firmware can't reach the hub for 5 minutes: rolls back, old firmware reports `rolled_back`, floor and keys unchanged.
31. A restart during the trial boot: rolls back, as above.
32. The hub answers the trial check-in with `token_revoked`: marked good.
33. A power cut during the download: the old firmware starts, and reports `failed` with `download`.

**Keys.**

34. A release with a `key_set` adding a key: after it is marked good, the new key is trusted.
35. A release with a `key_set` removing the key that signed it: afterwards that key is refused.
36. A release with a `key_set` that then rolls back: keys unchanged.
37. A replay of an earlier release, after a key-removing release with a higher counter. → `downgrade`
38. Factory reset: keys and floor kept.
39. A bundle with different keys to a display that has keys. → `firmware_keys_locked`. With the same keys, or none: accepted.
40. A bundle with keys to a display that has none: keys and floor set.

**Hub and app (tested against the real hub).**

41. Uploads refused: too large, truncated, trailing bytes, bad signature, wrong part hash, overlapping USB parts, repeated member names.
42. A fourth package for one device type removes the lowest version that isn't on offer.
43. An offer refused for a display whose report says wrong board, untrusted signer, floor too high or partition too small.
44. A publisher token can upload and list, and gets `forbidden` everywhere else.
45. A hub that serves a tampered package (a test double): the simulator refuses it. This is the test of the whole design.
46. The update form the hub serves is byte for byte the start of the uploaded file, without the USB parts.

## What was checked

Checked on 17/11/2026 (UTC), against official sources only.

| Fact | Source |
|---|---|
| ESP-IDF 5.5's Mbed TLS documentation points at Mbed TLS 3.6.3, and says the 3.x series supports TLS 1.2 and 1.3. The current stable ESP-IDF (6.1) points at Mbed TLS 4.1. | [ESP-IDF 5.5, Mbed TLS](https://docs.espressif.com/projects/esp-idf/en/v5.5/esp32/api-reference/protocols/mbedtls.html), [ESP-IDF stable, Mbed TLS](https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/protocols/mbedtls.html) |
| Mbed TLS 3.6 offers ECDSA, deterministic ECDSA and RSA signatures and the secp256r1 curve through PSA, and lists no EdDSA algorithm and no Edwards curve. | [Mbed TLS 3.6, `crypto_config.h`](https://github.com/Mbed-TLS/mbedtls/blob/mbedtls-3.6/include/psa/crypto_config.h) |
| EdDSA is listed as future work for Mbed TLS. | [Mbed TLS roadmap](https://mbed-tls.readthedocs.io/en/latest/project/roadmap/) |
| PSA ECDSA signatures are the raw fixed-size form, the older API uses DER, and PSA imports and exports a P-256 public key as an uncompressed point. | [Mbed TLS 3.6, PSA transition guide](https://github.com/Mbed-TLS/mbedtls/blob/mbedtls-3.6/docs/psa-transition.md) |
| ESP-IDF's Mbed TLS uses hardware SHA and, on chips that have it, hardware ECC, by default. | [ESP-IDF 5.5, Mbed TLS `Kconfig`](https://github.com/espressif/esp-idf/blob/v5.5/components/mbedtls/Kconfig) |
| App rollback: the option, the image states, one boot to confirm, rollback on any reset before confirming, the functions named above, and the advice to self-test quickly. OTA needs two app partitions and an OTA data partition. `esp_ota_end` validates the image. | [ESP-IDF 5.5, OTA](https://docs.espressif.com/projects/esp-idf/en/v5.5/esp32s3/api-reference/system/ota.html) |
| eFuse anti-rollback: 32 steps on the ESP32, 16 on the ESP32-S3, no factory or test partition. | The OTA pages for [ESP32](https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/ota.html) and [ESP32-S3](https://docs.espressif.com/projects/esp-idf/en/v5.5/esp32s3/api-reference/system/ota.html) |
| Secure Boot v2: RSA-3072 on the ESP32 and ESP32-S3, chip revision 3.0 for the ESP32, the USB loader in ROM turned off, updates must be signed with the burnt-in key. ESP-IDF's own "signed app without Secure Boot" mode trusts the key in the running app, and allows only one. | [Secure Boot v2, ESP32](https://docs.espressif.com/projects/esp-idf/en/v5.5/esp32/security/secure-boot-v2.html), [ESP32-S3](https://docs.espressif.com/projects/esp-idf/en/stable/esp32s3/security/secure-boot-v2.html) |
| Flash encryption: release mode stops plain firmware being flashed, plain data on an encrypted board stops it starting, and it doesn't by itself stop firmware being altered. | [ESP-IDF 5.5, flash encryption](https://docs.espressif.com/projects/esp-idf/en/v5.5/esp32s3/security/flash-encryption.html) |
| The app description block sits at a fixed offset in an app image and holds the version. | [ESP-IDF 5.5, app image format](https://docs.espressif.com/projects/esp-idf/en/v5.5/esp32s3/api-reference/system/app_image_format.html) |
| `ring` 0.17 signs and verifies fixed-length P-256 signatures, uses a random nonce, takes the public key as an uncompressed point, and makes PKCS#8 keys. | [`ring::signature`](https://docs.rs/ring/0.17.14/ring/signature/index.html), [`EcdsaKeyPair`](https://docs.rs/ring/0.17.14/ring/signature/struct.EcdsaKeyPair.html) |
| `espflash` 4.7 can report the chip and flash size, erase the flash, and write raw bytes at an address. | [`espflash::flasher::Flasher`](https://docs.rs/espflash/latest/espflash/flasher/struct.Flasher.html) |

**Why ESP-IDF's own signed-update mode isn't used.** It checks a new app against the single key built into the running app, in Espressif's own signature block. This design needs the key to come from the first install, more than one key, and a manifest that names the board and the security counter.

**Not yet checked, and to be confirmed when the firmware is built:**

- how long a P-256 verification takes on a chip with no ECC accelerator (expected to be well under a second);
- which Mbed TLS release each ESP-IDF 5.x version before 5.5 ships, and whether PSA is switched on by default in each;
- whether ESP-IDF refuses an app image built for a different chip when it validates it (the display's own device-type check doesn't depend on this);
- the eFuse anti-rollback step count on chips other than the ESP32 and ESP32-S3;
- `espflash`'s behaviour on each USB serial type, on Windows and macOS.
