# dh_core

The display's protocol and signed-update core: plain C11, no hardware, no ESP-IDF headers and no heap of its own. It builds as an ESP-IDF component and as a plain library for the host tests in `firmware/host-tests`, which run every snapshot vector in `protocol/v1/vectors`, every example in `protocol/v1/examples` and every firmware update vector in `protocol/v1/firmware`. The firmware itself links it from `firmware/main`.

It follows the reference display, `tools/display-simulator`, with `crates/hub-protocol` and `crates/firmware-package`, including some details the documents leave open (listed at the top of `src/dh_snapshot.c`, `src/dh_update_package.c`, `src/dh_update_state.c`, `src/dh_setup.c` and `src/dh_usb.c`).

## What's here

| Header | What it does |
|---|---|
| `dh_json.h` | A small, strict JSON reader for untrusted input. |
| `dh_snapshot.h` | `dh_read_response()`: an HTTP status and body in; what to do, and the snapshot as plain structs, out. Ages, staleness and reset countdowns on the hub's clock. |
| `dh_poll.h` | When to poll next (refresh ±10%, back-off from 15 s to 15 minutes ±20%, hourly), and the problem to show. Randomness comes from the caller. |
| `dh_hub.h` | Everything else said to the hub: the enrolment request and the heartbeat, the enrolment response, error answers from any route, the device ID and token with their stored form, which hub address and trust anchor to try next, when a heartbeat is due, and when the last good snapshot is worth saving. |
| `dh_text.h` | UTF-8, the text rules for titles and labels, and an exact NFC check. |
| `dh_time.h` | Timestamps in the wire form. |
| `dh_usb_line.h` | Splits the USB serial stream into JSON lines. |
| `dh_setup.h` | The provisioning bundle: its rules, the setup it becomes, and that setup's stored form. |
| `dh_usb.h` | Setup over USB: one line in, the answer out, with whatever must be stored written through the caller's store. A factory reset. |
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
- **What is kept.** Two things live in flash: the trusted keys with the security floor (`dh_fw_keys`, to go in an NVS namespace of its own, which a factory reset leaves alone) and the update record (`dh_fw_record`). Each has a stored form with a fixed layout and a format number (`dh_fw_keys_encode`, `dh_fw_record_encode`), because the firmware before and after an update must read each other's. Decoding checks every field. **A later format must stay readable by this firmware**, for as long as a display can go back to it: a record that doesn't decode reads as a new one, with a full attempt budget and no release waiting, and keys that don't decode read as none. Everything else (`dh_fw_session`) lives in memory for one start, and losing it at a restart is part of the rules.

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

A write that *fails* is a different thing from a power cut: it may have reached flash all the same, so the firmware can't know which row it is in. It doesn't guess: `main/storage_fw.c` stops all update work for the rest of that run, and the next start reads flash and lands in one of the rows above.

The keys and the floor only ever change in `dh_fw_marked_good()`, in `dh_fw_start()` finishing the same job, and in `dh_fw_bundle_apply()` for a display that has no keys.

### What isn't here

Fetching the offer from a snapshot is `dh_snapshot.h`'s job (`firmware_update`). Everything that touches hardware is the firmware's: NVS, `esp_http_client`, the `esp_ota_*` calls, the two time limits on a download (`dh_fw_download_overdue()` says when to give up) and sending the reports. That part is `main/hub.c`, `main/update.c` and `main/storage_fw.c`, and the host tests run the last two as well, over a stand-in for the board (`firmware/host-tests/glue`).

## Setup over USB

`dh_usb_handle_line()` takes one line from `dh_usb_line_feed()` and does everything the reference display does with it (`tools/display-simulator/src/usb.rs`): answers a `hello` with the display's own, and settles a `provision`. It writes the reply line into the caller's buffer and says what happened in words that are safe to log.

**The order a bundle is settled in.** The first that applies is the answer:

1. Storage isn't working, or can't say what it holds: `storage_failed`. (The display may give its own fixed sentence for the reply's message, through `storage_why`, a function asked at the moment of the refusal: the firmware uses it for a storage key that was never finished and for a factory reset that didn't finish. It is asked again if the store's `prepare` fails, where the firmware uses it for a key that couldn't be made just now.) The same code, with words of its own that say to restart the display and try again, when nothing is held but a write failed earlier in the run (`DH_HELD_RESTART_NEEDED`): what is stored isn't known until the display has restarted.
2. A setup or a device token is already held: `already_provisioned`. Nothing changes.
3. The bundle breaks a rule: `invalid_bundle`.
4. One of its firmware keys isn't a valid P-256 point: `invalid_bundle`. Nothing is stored.
5. The display trusts a different set of firmware keys: `firmware_keys_locked`. The same set in any order, or no `firmware` member, is fine.
6. A write fails: `storage_failed`.
7. Otherwise `provisioned`.

**The rules** are `Bundle::problems()` in `crates/hub-protocol/src/provisioning.rs`, in its order: the format and version, 1 to 4 hosts of 1 to 253 bytes with no white space or control character, a port that isn't 0, the fixed server name, 1 or 2 anchors whose PEM starts `-----BEGIN CERTIFICATE-----` and is at most 4096 bytes with 64 lower-case hex digits of `spki_sha256`, a secret of 1 to 64 bytes, an SSID of 1 to 32 bytes, a password that is empty or 8 to 63 bytes, the `firmware` member's own rules, and 12288 bytes in all. That last count is exact: the reference counts the bundle as serde_json writes it out again, and `dh_setup.c` works that length out from the same members in the same order, each string escaped serde_json's way.

**The store.** The caller passes a `dh_setup_store`: five functions over whatever it keeps things in (encrypted NVS on the chip, a fake in the tests), and an optional sixth, `prepare`. Nothing is changed before step 6. There the store's `prepare` is called, if it has one: once for a bundle that has passed every check, before its first write, and never for a line that is refused or ignored. On the chip it is where a new board's storage key is made, which is permanent, so it must not be set off by anything less than a bundle that is about to be stored; if it fails the answer is `storage_failed` with nothing written. Then the handler works on copies and writes the firmware keys (only for a display that had none), then the update record (only if the attempt budget wasn't full), then the setup, last. Each copy is taken into the caller's memory as soon as its write succeeds, so memory and flash always agree, and a failed write never leaves a setup behind: the display still reads as not set up, and the same bundle can be sent again. (If the keys were written and a later write failed, the display keeps those keys. They came from the package the app had just installed, and the next bundle names the same ones.)

**A factory reset** is `dh_usb_factory_reset()`: it calls the store's `erase_setup` and nothing else. It is never given the keys or the record, so it can't change them.

**The stored form** of a setup (`dh_setup_encode()`, `dh_setup_decode()`) is one blob of at most 9464 bytes: a format number, then each field with its length. Decoding checks every field against the bundle's own rules and that every byte is accounted for, and encoding refuses a setup that decoding would refuse. A setup holds secrets: `dh_setup_wipe()` and `dh_usb_line_wipe()` clear a copy that is no longer needed, and nothing in the core ever puts bundle content in a reply or a `why`.

**Where it differs from the reference**, on purpose:

- A line the reference can't read into its message type at all gets no answer, as in the reference: a member that is missing, repeated or of the wrong type, a `version` or `security_counter` that isn't a plain integer fitting 32 bits, a `port` that doesn't fit 16 bits, an `expires_at` that isn't a real date. But a line with bytes that aren't UTF-8 is ignored here, where the reference swaps them for U+FFFD and reads on; and an object written as an array of its members in order, which serde also accepts, is unreadable here.
- An error's `message` names the first broken rule in fixed words (at most 200 bytes, as the schema asks). The reference lists every problem, and passes on a storage error's own text.
- The reference writes the bundle first, then the keys and the record, so a failure after the first write leaves it set up although it answered `storage_failed`. Here the setup is written last.
- A store that can't be read refuses a bundle with `storage_failed` before anything else. The reference has no such state before the keys are read.

A `provision` whose bundle is of the wrong shape gets no answer, from the reference or from this, as the protocol's "Over USB" says.

## Talking to the hub

`dh_hub.h` is what a display needs once it is set up, beside the snapshot reader and the poll schedule. Like the rest, it follows the reference (`tools/display-simulator/src/paired.rs` and `main.rs`, and `crates/hub-client`).

- **The requests.** `dh_enrol_request()` and `dh_heartbeat_request()` write the JSON bodies, with the `firmware` member (key ids, security floor, spare slot size, attempts left) whenever the keys and the record are known. The same code writes the USB `hello`, so the three can't drift apart. A secret is written as any text may be: quotes, control characters and longer characters all survive. `dh_firmware_report_request()` writes a firmware report and `dh_firmware_path()` the path a download is asked for at, which takes nothing but a package id of 64 lower-case hex digits.
- **The answers.** `dh_read_enrol_response()` takes any 2xx status with a readable body as success, and groups anything else as `dh_read_error_response()` does: the four groups of the protocol's error table, for any route. Both stop at 8 KiB.
- **The device ID and token** are taken as 1 to 64 and 1 to 128 printable ASCII characters with no space, which is wider than the schema (so a hub may change how it makes them) and narrow enough to put a token in a header as it is. `dh_device_encode()` and `dh_device_decode()` are the stored form.
- **Which address** (`dh_hub_walk`): the hub address that answered last time first (kept in memory for one start, never stored), then the others in the bundle's order; at an address, each trust anchor while the certificate doesn't match; an address with no answer isn't tried with the other anchor; the first real answer ends it. With no answer anywhere, the problem is the last address tried, as in the reference.
- **Heartbeats** (`dh_heartbeat_due()`): after start-up, hourly, and when the attempts left change; a change the hub didn't take is sent again every five minutes, never at every poll. The reference's own test cases are in `hub_tests.c`.
- **Saving the last good snapshot** (`dh_snapshot_bodies_match()`, `dh_snapshot_save_due()`): only when something other than `generated_at` differs from the saved copy, and then at most hourly.

`dh_poll.h` has one problem the reference words but the handoff's list doesn't: `DH_PROBLEM_SETTINGS`, "This display's settings are wrong", for a setup with no Wi-Fi details or no certificate that can be read.

**Where it differs from the reference**, on purpose: an ID or token outside the characters above makes the response unreadable (the reference takes any string); an error answer over 8 KiB counts as having no body (the reference fails the read); at one address, a certificate mismatch from the first anchor isn't forgotten if the second anchor's try gets no answer; the address that answered last is tried first (the reference always walks in the bundle's order), because on a board a name that can't be looked up costs several seconds at every request; and a final firmware report is settled only by a `2xx` answer, by `404` with `not_found` (the hub has no such offer) or by `token_revoked` or `unknown_device` with a 4xx status (`dh_firmware_report_settled()`). After anything else it is sent again following a later poll: no answer, a `5xx`, `rate_limited`, the faults the protocol says to retry, a code this firmware doesn't know, an answer with no error body. The reference counts any answer at all as delivered. Because a waiting final report holds back new offers, the firmware bounds that: after the hub has refused the same report 10 times (`DH_FW_REPORT_MAX_REFUSALS`), it gives the report up and treats it as settled. Only lasting refusals count (`dh_firmware_report_refused()`): a 4xx answer with one of the retry faults, an unknown code or no error body. A hub that is busy or away never counts, however long it lasts: no answer, any 5xx, 408, 429, `rate_limited` and `unavailable`. The count is kept in memory, so it starts again at a restart. A report that can't be written at all is given up at once.

## Memory

A `dh_snapshot` is about 38 KB and a `dh_text_scratch` about 4.5 KB; keep them in static memory or PSRAM, not on a task's stack. Reading uses about 1 KB of stack of its own (measured with `-fstack-usage` for the ESP32-S3), plus whatever the C library's `strtod` and `snprintf` take. Read into a spare snapshot and swap it in on `DH_OUTCOME_SHOW`, so the last good one survives a refusal.

A request body needs `DH_HUB_REQUEST_MAX` (1024) bytes, and a `dh_device` is under 200. `dh_snapshot_bodies_match()` parses both bodies in place and copies nothing.

A `dh_setup` is about 9.5 KB and a `dh_usb_line_reader` 16 KB: keep them in static memory or on the heap. Handling a line needs room for the reply (`DH_USB_REPLY_MAX`, 768 bytes) and under 1 KB of stack of its own, plus Mbed TLS's for each key in a bundle.

A `dh_fw_download` is about 12 KB (the manifest's 4 KB, room to check its text, and the manifest as read); keep it in static memory too. The update core's own functions use under 200 bytes of stack each on the ESP32-S3 (measured with `-fstack-usage`), on top of the JSON reader's. Checking a point or a signature is Mbed TLS's work: it takes a few kilobytes of heap and stack for a moment, once per download and once for each key in a `key_set` or a bundle.

## Testing

See `firmware/host-tests` for the host tests and `firmware/fuzz` for the fuzz targets.
