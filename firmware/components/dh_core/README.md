# dh_core

The display's protocol and signed-update core: plain C11, no hardware, no ESP-IDF headers and no heap of its own. It builds as an ESP-IDF component and as a plain library for the host tests in `firmware/host-tests`, which run every snapshot vector in `protocol/v1/vectors`, every example in `protocol/v1/examples` and every firmware update vector in `protocol/v1/firmware`.

It follows the reference display, `tools/display-simulator`, with `crates/hub-protocol` and `crates/firmware-package`, including some details the documents leave open (listed at the top of `src/dh_snapshot.c`, `src/dh_update_package.c` and `src/dh_update_state.c`).

## What's here

| Header | What it does |
|---|---|
| `dh_json.h` | A small, strict JSON reader for untrusted input. |
| `dh_snapshot.h` | `dh_read_response()`: an HTTP status and body in; what to do, and the snapshot as plain structs, out. Ages, staleness and reset countdowns on the hub's clock. |
| `dh_poll.h` | When to poll next (refresh ±10%, back-off from 15 s to 15 minutes ±20%, hourly), and the problem to show. Randomness comes from the caller. |
| `dh_text.h` | UTF-8, the text rules for titles and labels, and an exact NFC check. |
| `dh_time.h` | Timestamps in the wire form. |
| `dh_usb_line.h` | Splits the USB serial stream into JSON lines. |
| `dh_limits.h` | The protocol's bounds. |
| `dh_update.h` | Signed firmware updates: the package header and manifest, one download checked as it arrives, the attempt budget, offers, the trial boot, reports, the provisioning bundle's keys, and the stored forms of what a display keeps. |
| `dh_crypto.h` | SHA-256 a piece at a time and ECDSA P-256 verification, from Mbed TLS. |

Nothing keeps state between calls: every function works on what the caller passes in. That includes `dh_crypto`, which uses Mbed TLS's own SHA-256 and ECDSA calls rather than its PSA ones, because PSA keeps a key store that every caller shares and ESP-IDF builds Mbed TLS without the locks for it by default.

## The JSON reader

`dh_json_parse()` checks the whole document in one pass, with a fixed stack of one bit per nesting level, so it never recurses or allocates. It refuses anything that isn't RFC 8259 JSON, bytes that aren't UTF-8 (overlong forms and surrogates included), a `\u` escape that is half a surrogate pair, raw control characters in strings, nesting past the limit (127 by default, as serde_json allows), a body over the size limit, and any number beyond the range of a double. Two options make it stricter, for signed manifests: `DH_JSON_NO_DUPLICATES` refuses an object that repeats a member name (compared after decoding escapes), and `DH_JSON_PLAIN_INTEGERS` refuses any number with a sign, fraction or exponent. `dh_json_number_form()` reports the same for one number.

After that, nothing is copied: a value is just its offset in the buffer. The readers find members (`dh_json_object_find()` says how many there are, so a caller can refuse repeats of the members it knows), walk arrays, decode strings into the caller's buffers and convert numbers. Because the parse has already proved the document well formed, they can skip over values without checking them again. Numbers are rounded by `strtod`, given at most 40 significant digits and an exponent.

The `dh_json_serde_*` functions say what serde_json would make of a value, which the snapshot reader needs to match the reference exactly: its 3072-byte page limit counts the page as serde writes it out again.

## Text and NFC

Titles, labels and notes must be in Unicode NFC, as the hub checks with the `unicode-normalization` crate. `dh_text_is_nfc()` is exact: a quick check settles most text, and anything that might compose or reorder is decomposed, reordered and composed again, one segment at a time, then compared. The tables in `src/dh_unicode_tables.c` (about 14 KB of flash) come from the same crate release the hub pins, written by `tools/unicode-tables`:

```sh
cd firmware/components/dh_core/tools/unicode-tables
cargo run --release > ../../src/dh_unicode_tables.c
```

The generator also checks the assumptions the C code makes about the data. Run it again whenever `crates/hub-protocol` moves to a new `unicode-normalization` release.

## Signed updates

`dh_update.h` is the display's side of `docs/firmware-updates.md`. Its comment at the top lists the calls for one update in order. The rules it keeps:

- **Order.** A download is pushed in pieces of any size. The header's fixed fields are checked first, then its key against the display's own stored keys, then the signature over the manifest's exact bytes with the stored copy of that key, and only then is the manifest parsed and checked against the display (device type, hardware revision, security floor, the `key_set` rule, the spare partition's size, and the lengths against the `Content-Length`). Nothing is handed over to be written before all of that has passed, and no byte past the update form is ever taken.
- **A small, fixed state.** A `dh_fw_download` holds the header, the manifest (4096 bytes at most) and a running SHA-256, never any of the image, however large the image is.
- **No I/O.** The core never touches flash, the network or a clock. It tells the caller what to do (open the spare partition, write these bytes, finish, abandon) and the caller says what happened. Time is the caller's own running time in whole seconds since this start.
- **What is kept.** Two things live in flash: the trusted keys with the security floor (`dh_fw_keys`, to go in an NVS namespace of its own, which a factory reset leaves alone) and the update record (`dh_fw_record`). Each has a stored form with a fixed layout and a format number (`dh_fw_keys_encode`, `dh_fw_record_encode`), because the firmware before and after an update must read each other's. Decoding checks every field. Everything else (`dh_fw_session`) lives in memory for one start, and losing it at a restart is part of the rules.

### Save points

A function that changes what is kept returns `DH_FW_SAVE_KEYS`, `DH_FW_SAVE_RECORD` or both. The caller writes what is named, the keys first, each as one blob in one write, before doing anything else. Three of these saves guard a step that must come after them:

| After | Save | Only then |
|---|---|---|
| `dh_fw_on_offer()` says `DH_FW_OFFER_ATTEMPT` | the record (one attempt spent, with the offer id) | request the download |
| `dh_fw_attempt_installed()` | the record (the release waiting for its trial boot) | `esp_ota_set_boot_partition()` and restart |
| `esp_ota_mark_app_valid_cancel_rollback()` | what `dh_fw_marked_good()` names: the keys, then the record | report `installed` |

A power cut anywhere is safe, because `dh_fw_start()` settles whatever it finds at the next start:

| The power went | The next start finds | And does |
|---|---|---|
| Before an attempt was saved | the record as it was | Nothing: no request was made, and nothing was spent. |
| During a download | an attempt in progress | Reports `failed` with `download`. The attempt stays spent. |
| After the image was verified, before the record was saved | the same | The same. The spare partition was never set to boot. |
| After that save, before the partition was set to boot | a release waiting, in a partition that isn't the one running | Drops its `key_set`, leaves the floor alone and reports `rolled_back`. |
| During the trial boot | the same (the bootloader went back) | The same. The trial boot itself saves nothing. |
| After the image was marked good, before the keys or the record were saved | a release waiting, in the partition that is running, no longer on trial | Raises the floor, takes the `key_set` and reports `installed`: what `dh_fw_marked_good()` does, again. It is safe to repeat. |
| Before a credit hour, or a report's answer, was saved | the record as it was | The hour is counted again from zero, or the report is sent again. |

The keys and the floor only ever change in `dh_fw_marked_good()`, in `dh_fw_start()` finishing the same job, and in `dh_fw_bundle_apply()` for a display that has no keys.

### What isn't here

Fetching the offer from a snapshot is `dh_snapshot.h`'s job (`firmware_update`). Everything that touches hardware is the firmware's: NVS, `esp_http_client`, the `esp_ota_*` calls, the two time limits on a download (`dh_fw_download_overdue()` says when to give up) and sending the reports.

## Memory

A `dh_snapshot` is about 38 KB and a `dh_text_scratch` about 4.5 KB; keep them in static memory or PSRAM, not on a task's stack. Reading uses about 1 KB of stack of its own (measured with `-fstack-usage` for the ESP32-S3), plus whatever the C library's `strtod` and `snprintf` take. Read into a spare snapshot and swap it in on `DH_OUTCOME_SHOW`, so the last good one survives a refusal.

A `dh_fw_download` is about 12 KB (the manifest's 4 KB, room to check its text, and the manifest as read); keep it in static memory too. The update core's own functions use under 200 bytes of stack each on the ESP32-S3 (measured with `-fstack-usage`), on top of the JSON reader's. Checking a point or a signature is Mbed TLS's work: it takes a few kilobytes of heap and stack for a moment, once per download and once for each key in a `key_set` or a bundle.

## Testing

See `firmware/host-tests` for the host tests and `firmware/fuzz` for the fuzz targets.
