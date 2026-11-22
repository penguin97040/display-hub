# Firmware handoff

This is the brief for building display firmware against protocol v1 as it stands on 18/11/2026. It has two kinds of reader:

- **The official firmware**, in this repository under [`firmware/`](../firmware/README.md): C with ESP-IDF 5.5.6, for the Waveshare ESP32-S3-Touch-AMOLED-1.8 V2. It uses `protocol/v1/` and the tools here in place, and is the starter project for a maker's own. Its README, and [`dh_core`'s](../firmware/components/dh_core/README.md), say where it differs from the reference display and why.
- **Makers** writing firmware for their own displays, in their own repositories, with or without an AI agent. Copy what you need from here at a pinned commit (below), rather than depending on this repository as a submodule.

[`docs/protocol-v1.md`](protocol-v1.md) is the contract and [`docs/firmware-updates.md`](firmware-updates.md) is the design for signed updates. This brief puts them in the order a firmware author meets them, adds what was learnt on a real board, and gives the choices the official firmware has made. Where this brief and those two documents differ, they win. Where any document and the code differ, the code is the truth: tell the project.

[BUILDING-A-DISPLAY.md](../BUILDING-A-DISPLAY.md) has the rules not to break and the finish line for everything a maker builds.

## What to use from here

All of these are under `protocol/v1/` unless noted.

| What | Where | Use |
|---|---|---|
| The contract | `docs/protocol-v1.md` | The source of truth. Frozen on 18/11/2026: from now on v1 changes only by optional additions. |
| Signed updates | `docs/firmware-updates.md` | The package format, every check a display makes, the attempt budget, the trial boot and the threat model. |
| JSON Schemas | `snapshot`, `enrol-request`, `enrol-response`, `heartbeat`, `error`, `provisioning`, `usb`, `firmware-report` (`*.schema.json`) | Check your structs and the messages you send. |
| Examples | `examples/valid` (9), `examples/invalid` (32), `examples/device` (11), `examples/usb` (7) | Parser tests. Five invalid examples are tolerated by displays (see the protocol, "Conformance"). |
| Snapshot vectors | `vectors/` (57 cases, [format](../protocol/v1/vectors/README.md)) | Port to unit tests: a hub response in, what the display does and shows out. |
| Firmware vectors | `firmware/` (79 cases: 49 `package`, 12 `offer`, 10 `trial`, 8 `provision`; [format](../protocol/v1/firmware/README.md)) | Port to unit tests: package checks, the attempt budget, the trial boot, bundles over USB. |
| TLS reference client | `tools/mbedtls-interop/client.c` | An mbedTLS 3.6 client set up as ESP-IDF's HTTP client should be, tested against the real hub. |
| Reference display | `tools/display-simulator` | Behaves as firmware must, signed updates included. Run it beside a board to compare. |
| Signing tool | `apps/hub-firmware` | Makes keys, and signs and checks packages. |
| Official firmware | `firmware/` ([README](../firmware/README.md)) | A whole display firmware in ESP-IDF, with its protocol and signed-update core in plain C (`firmware/components/dh_core`) tested on a computer against every vector. |

**Pin a commit.** A maker records the display-hub commit they copied from, and copies again only on purpose. The official firmware in `firmware/` reads `protocol/v1/` directly, so its tests always run against the current files.

## The board

Waveshare **ESP32-S3-Touch-AMOLED-1.8**: [documentation](https://docs.waveshare.com/ESP32-S3-Touch-AMOLED-1.8), [examples](https://github.com/waveshareteam/ESP32-S3-Touch-AMOLED-1.8). Two hardware revisions exist, marked on the back of the board, with different display and touch controllers:

| Revision | Display controller | Touch controller | `hardware_revision` to report |
|---|---|---|---|
| V1 | SH8601 | FT3168 | `waveshare-amoled-1.8-v1` |
| V2 | CO5300 | CST820 | `waveshare-amoled-1.8-v2` |

- The official firmware supports **V2 only**. It tells the revisions apart at start-up by which touch chip answers: a V1 board reports `waveshare-amoled-1.8-v1` and says on its screen that it isn't supported, and a board whose touch chip doesn't answer reports `waveshare-amoled-1.8-unknown`. Neither is ever reported as a V2. Firmware that supports V1 as well can choose the drivers by the same probe, or at build time. Only V2's screen and touch have been driven from this project, so confirm V1's controllers against Waveshare's documentation.
- The display is 368 × 448 (the simulator and the official firmware draw at that size).
- A V2 board reported 16 MB of flash to ESP-IDF. Its chip is an ESP32-S3 with 8 MB of PSRAM, by the product's description.
- The board runs on USB-C power. It talks to the hub over 2.4 GHz Wi-Fi, and uses USB only for setup and for the first install.
- It has **PWR** and **BOOT** buttons and **no RESET button**. To start the chip's built-in loader (download mode), hold BOOT while plugging the cable in.

## A display's life, in short

1. **No setup:** wait on USB for a provisioning bundle.
2. **Set up:** join Wi-Fi, enrol once, keep the device token.
3. **Running:** poll the snapshot, draw its pages, send a heartbeat now and then, and act on a firmware offer if there is one.
4. **Trouble:** keep the last good pages on screen and say plainly what is wrong.
5. **Factory reset**, on the device itself: back to step 1, keeping the firmware keys.

## Waiting for setup (USB)

With no stored bundle, the display shows that it needs setting up from the desktop app, and listens on its USB serial port (CDC-ACM or USB-Serial-JTAG) at 115200 baud. The exchange is in `docs/protocol-v1.md`, "Over USB": UTF-8 JSON, one object per line ending in `\n`.

1. **Answer every `hello`** with a `hello` of your own: `device_type`, `firmware_version`, `hardware_revision`, `protocol_version: 1`, `provisioned`, and the `firmware` member if you support signed updates (`examples/usb/display-hello-with-firmware.json`). The app repeats `hello` every 2 seconds for up to 10 seconds, because the board may restart when the port opens. Each answer must arrive within 10 seconds.
2. **On `provision`,** the official firmware takes these steps, in this order. The reference display (`tools/display-simulator/src/usb.rs`) does the same, except where noted:
   1. Storage that can't be read, or that can't say what it holds: answer `storage_failed`, and change nothing. (The reference has no such state.)
   2. Holding a bundle or a device token already: answer `already_provisioned`, and change nothing.
   3. The bundle breaks a rule: answer `invalid_bundle`. Check the same rules as `Bundle::problems()` in `crates/hub-protocol/src/provisioning.rs`: format and version, 1 to 4 hosts, a port, the fixed server name, 1 or 2 PEM certificates under 4 KiB with their 64-digit `spki_sha256`, a secret of 1 to 64 characters, an SSID of 1 to 32 bytes and a password that is empty or 8 to 63 characters, the `firmware` member's rules, and 12 KiB at most in all.
   4. The bundle's `firmware.trusted_keys` holds a key that isn't a valid P-256 point: `invalid_bundle`, storing nothing.
   5. The display already trusts firmware keys, and the bundle names a different set: `firmware_keys_locked`. The same set in any order, or no `firmware` member, is fine and changes no keys.
   6. Otherwise store the bundle. A display with no keys takes the bundle's keys, and its `security_counter` as the floor. Accepting any bundle refills the attempt budget to 5. Answer `provisioned`, or `storage_failed` if a write failed. Write the setup itself **last**, after the keys and the update record, so a failed write never leaves a display that reads as set up. The cost: after a failed later write, the display already trusts that bundle's keys, so the same bundle sent again is taken and one naming other keys gets `firmware_keys_locked` until **Install firmware** erases it. (The reference writes the bundle first, so a later failed write leaves it set up although it answered `storage_failed`.)

   A line that isn't a readable `hello` or `provision` (a member missing, repeated or of the wrong type) gets no answer at all, whatever the display holds. The official firmware also ignores a line that isn't valid UTF-8, where the reference reads on with the bad bytes replaced, and its error `message` is one fixed sentence of at most 200 characters, where the reference's `invalid_bundle` message lists every problem. [dh_core's README](../firmware/components/dh_core/README.md#setup-over-usb) has the details.
3. **Ignore any line that doesn't start with `{`,** and drop any line over 16 KiB. Boot logs on the same port are harmless.
4. **Read the port.** Firmware that never reads its serial port can make the app's write time out (on Windows: `os error 121`, "The semaphore timeout period has expired"). Keep a task reading it whenever the display is waiting for setup.

The `firmware` rules come from [firmware-updates.md](firmware-updates.md), "What each action may change". The `provision-*` vectors check each case.

## Joining and enrolling

1. Join the Wi-Fi network in the bundle. If its `wifi` is `null`, keep the settings you have.
2. `POST /v1/device/enrol` with the bundle's one-time secret and your `device` fields (with `firmware` if you support updates). Try each `hub.hosts` entry in order; see "Finding the hub" in the protocol. (The official firmware tries the address that answered last first, for every request, then the rest in order.) Move on to the next host after a connection failure or a certificate mismatch; stop at the first answer from the real hub.
3. Store the returned `device_id` and `device_token`. **Erase the enrolment secret** once enrolment has succeeded or been refused. On `enrolment_used` or `enrolment_expired`, show that setup must be redone. The secret lasts 10 minutes, so enrol straight after setup.

## Every poll

1. `GET /v1/device/snapshot`, with `Authorization: Bearer <device token>` and `X-Display-Protocol: 1`.
2. Refuse a body over 32 KiB **before** parsing it.
3. Accept the snapshot only if `protocol_version` is 1, `message_type` is `dashboard.snapshot`, `refresh_after_seconds` is 30 to 3600, there are at most 10 pages, and every page's common fields (`id`, `title`, `updated_at`, `stale_after_seconds`, `note`) follow the rules with no repeated `id`. Otherwise keep the last good snapshot and say the hub sent something unreadable.
4. Draw exactly the pages given, in order. An empty list means "no pages yet". Keep the current page by `id` when pages move.
5. Save the snapshot as the last good one, only when it has changed, to spare the flash.
6. Measure ages and countdowns on the hub's clock: `generated_at`, plus a monotonic timer since the response arrived. Never use the display's own date.
7. If the snapshot has a `firmware_update` member, see [Signed firmware updates](#signed-firmware-updates-the-displays-side). A member you can't make sense of reads as absent; the pages still show.
8. Wait `refresh_after_seconds` ±10% before the next poll.

### Pages

Every page has `id`, `title`, `kind`, `data_quality`, `updated_at`, `stale_after_seconds`, an optional `note`, and `data`. There is no `dashboard_id` and no per-page `metrics`: what a page holds is in `data`, and `kind` says how to read it.

- **The four ready-made shapes:**
  - `meter`: 1 to 6 bars, each with an `id`, a `label`, a `percent` from 0 to 100 or `null`, and an optional `resets_at`;
  - `big_number`: one `value` (±1e15, or `null`), optional `decimals` (0 to 3), `unit` and `caption`;
  - `text`: one paragraph of 1 to 280 characters;
  - `list`: 0 to 8 lines, each with `text` and an optional `detail`.
- **Custom kinds** look like `name/version`, for example `nearby-flights/1` or `org.example.board/2`, with a version from 1 to 9999. The version is part of the kind: a display that knows `/1` treats `/2` as unknown. `data` is any JSON object nested no more than 8 levels. What it means is agreed between whoever writes the hub-side source and whoever writes the firmware that draws it.
- **The "can't show this" card.** When a page's kind is unknown, its `data` breaks its shape's rules, it has `data` while `unavailable`, or it is over 3072 bytes, show the page's `title` and then exactly:

  > This display can't show this page.

  and, for an unknown kind only, a second line:

  > It may need a firmware update.

  Nothing else about the page: no figures, note or age. One card never costs the display the other pages.
- **An unavailable page with no `data`** isn't a card: show its title and `note`, whatever its `kind`.
- **Size limits:** 32 KiB for the whole snapshot, 3072 bytes for each page as compact JSON, 10 pages. Titles and labels are up to 48 characters, notes up to 96, counted as characters, not bytes. All text is plain Unicode (NFC), with no control characters.
- **Be lenient:**
  - ignore fields you don't know, in the page and inside `data`;
  - read an unknown `data_quality` as `unavailable`;
  - read a missing or `null` `percent` as unknown;
  - treat an omitted optional field and `null` the same.
- A JSON parser may refuse a whole body nested deeper than about 100 levels, or with a number outside the range of a double. A hub never sends either.

The snapshot vectors give the exact outcome for every case, card lines included. The reference display supports no custom kinds, so in the vectors every custom kind is a card. Firmware that draws a custom kind tests that kind itself.

### Heartbeat

`POST /v1/device/heartbeat`, with the same `device` fields as enrolment, plus `firmware` if you support updates:

```json
"firmware": { "key_ids": ["<64 hex digits>"], "security_floor": 3, "slot_bytes": 3145728, "attempts_left": 5 }
```

- `key_ids`: the SHA-256 of each trusted public key's 65 bytes, 0 to 3, no repeats.
- `security_floor`: the highest security counter the display has confirmed.
- `slot_bytes`: the size of the spare app partition, the one an update would be written to.
- `attempts_left`: optional but worth sending, 0 to 5.

Send one after start-up, then hourly. Also send one as soon as `attempts_left` changes. If the hub doesn't take that one, try again no more often than every five minutes until it does. **Never send a heartbeat on a trial boot** (below). The hub records at most one a minute, and answers `204`.

### Errors and back-off

Errors are `{"error": {"code": "...", "message": "..."}}`. Act on the `code`, by its group:

| Group | Codes | What to do | Next try |
|---|---|---|---|
| `reprovision` | `token_revoked`, `unknown_device`, `enrolment_used`, `enrolment_expired` | Show that the display needs setting up again from the desktop app. | Hourly |
| `incompatible` | `incompatible_version` | Show that the hub and display software don't match. | Hourly |
| `retry` | `rate_limited`, `unavailable`, and any code you don't recognise | Keep the last good data. | Back off |
| `fault` | `unauthorised`, `forbidden`, `bad_request`, `payload_too_large`, `not_found`, `conflict` | Keep the last good data, and log the code. | Back off |

An error with no JSON body, such as a captive portal's page, counts as `unavailable` for a 5xx status and as a fault otherwise.

**Back-off** after a failure: 15 s, then 30 s, 60 s and so on, doubling to at most 15 minutes, each with ±20% jitter. After a success, wait `refresh_after_seconds` ±10%.

### On screen

- Every page shows its data quality: DEMO, REPORTED, ESTIMATE, MANUAL or UNAVAILABLE.
- A `null` share reads "unknown", and a `null` figure reads as a dash: **never 0**.
- A stale page says so, with its age.
- A passed reset reads "reset due" instead of a countdown.
- An unavailable page shows its `note` and no figures.
- Swiping left and right moves between pages.
- Problems use plain words:
  - can't reach the hub;
  - the hub sent unreadable data;
  - the hub is busy;
  - setup is needed;
  - versions don't match;
  - something else is answering as the hub (a certificate mismatch).
- After a restart, show the saved snapshot. Its age is only known if the display has a trustworthy time since then, from SNTP for example. Otherwise mark it "saved; age unknown" until the next successful poll.
- While an update is downloading, say so, and keep showing the pages until the restart.

## Signed firmware updates: the display's side

Signed updates are optional in v1, but a display without them can only be updated over a cable. The official firmware supports them. The full rules, with the reasons, are in [firmware-updates.md](firmware-updates.md), "What a display must do": follow that document, and use this section as the checklist.

### Build settings

- [ ] `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE` on.
- [ ] A partition table with an OTA data partition and two OTA app partitions (see [The partition layout](#the-partition-layout)).
- [ ] Verify signatures with Mbed TLS: `psa_verify_hash` with `PSA_ALG_ECDSA(PSA_ALG_SHA_256)` takes the 64-byte `r‖s` signature and the 65-byte public key as they are. Accept no other algorithm.
- [ ] Download with `esp_http_client` and the `esp_ota_*` functions, not `esp_https_ota`: the update form has a header in front of the image.

### What you store

The trusted keys (0 to 3) and the security floor, in an NVS namespace of their own. An update record: the last offer id acted on, the attempt budget (attempts left 0 to 5, credit hours 0 to 3), the release waiting for its trial boot (version, security counter, any `key_set`), any attempt in progress, and the last result and whether the hub has answered it.

A write of the keys or the record that fails may still have reached flash, so don't guess which record flash holds. The official firmware stops all update work until it restarts: no offers, saves or update reports, and no `firmware` member in heartbeats, enrolment or the USB `hello`. A display that isn't set up refuses a bundle with `storage_failed` and a message saying to restart it. The pages and polls carry on, and the next start reads flash and settles whatever is there.

### The attempt budget

- [ ] Starts at 5. Saved in NVS with the update record.
- [ ] **Spent before each download request:** with at least one attempt left, save the reduced budget **and** the `offer_id` in a single write, then request. With none left, request nothing, report nothing, and leave the offer open.
- [ ] **Earned back by running time only:** while below 5, count running time with `esp_timer_get_time` (it starts at zero at every start). Save one credit hour for each full hour; at 4 credit hours, add one attempt and go back to 0. Time not yet saved is lost at a restart. At 5, count and write nothing.
- [ ] **Nothing else changes it:** not an attempt's result, a restart, the date, or anything from the hub. It goes back to 5 only when a bundle is accepted over USB, or the flash is erased by the app's **Install firmware**.
- [ ] Never act on the `offer_id` you last acted on. A refusal from the offer alone (`no_trusted_keys`, `too_large`) spends nothing, writes nothing, and is remembered only until the next start.

### Acting on an offer

The snapshot's `firmware_update` is `{"offer_id", "package_id", "size"}`. `size` is the length of the update form: header, manifest and image. Check in this order; nothing is written to the spare partition before step 8.

0. The `offer_id` you last acted on, or one refused from the offer alone since this start: do nothing. While on a trial boot, or with a final report still unanswered, act on nothing new.
1. No trusted keys: refuse (`no_trusted_keys`). No request, nothing spent.
2. `size` over 137 + 4096 + `slot_bytes`: refuse (`too_large`). No request, nothing spent.
3. Spend an attempt (above), then `GET /v1/device/firmware/{package_id}` over the same pinned TLS, with the token. `Content-Length` must equal `size` (`too_large` otherwise, without reading the body). A `not_found`, `rate_limited` or `unavailable` answer, or no answer, is `failed` with `download`.
4. Read 137 bytes: magic `DHFW`, format 1, algorithm 1, `manifest_len` 2 to 4096 (`bad_format`).
5. The header's public key must equal one of **your stored keys**, all 65 bytes (`untrusted_key`). Verify with your stored copy.
6. Read the manifest and **verify the signature before parsing it** (`bad_signature`). The signed bytes are `display-hub.firmware.v1`, a zero byte, header bytes 0 to 72, then the manifest exactly as sent.
7. Parse the manifest (`bad_format` for a missing or mistyped field, a repeated member name, or a number with a sign, fraction or exponent). Then:
   - `device_type` equal to yours, byte for byte (`wrong_device_type`);
   - `hardware_revisions` containing yours exactly, never as a prefix (`wrong_hardware`);
   - `security_counter` at least your floor (`downgrade`);
   - a `key_set` that leaves out any key you trust now needs a counter strictly above the floor (`downgrade`);
   - `image_size` fits the spare partition (`too_large`), and 137 + `manifest_len` + `image_size` equals `Content-Length` (`bad_format`).
8. Send the `downloading` report once, unless you sent it just before the request in step 3, as the official firmware does so that only one TLS session is ever open (the reference sends it here). Only now `esp_ota_begin` the spare partition. Write exactly `image_size` bytes as they arrive, hashing them, and read no more. A hash that doesn't match `image_sha256`: `esp_ota_abort`, `failed` with `image_hash`.
9. `esp_ota_end` (`failed` with `flash` if it, or any write, fails). Save the update record, `esp_ota_set_boot_partition`, and restart.

Give up on a download that sends nothing for 30 seconds, or takes more than 15 minutes in all, by the monotonic timer (`failed` with `download`). Downloads never resume: the next attempt starts from the beginning. Feed your parser pieces of any size: the vectors' reference feeds 97 bytes at a time.

### The trial boot

New firmware on its first start (`esp_ota_get_state_partition` says `ESP_OTA_IMG_PENDING_VERIFY`):

- [ ] Starts its screen and storage, joins Wi-Fi, and **checks in**: an authenticated request over pinned TLS that doesn't name the new version, such as fetching the snapshot. The simulator polls every 15 seconds while on trial.
- [ ] Counts as **good** when the hub answers a check-in with success, or with `token_revoked` or `unknown_device`. Any other error isn't enough. Then it calls `esp_ota_mark_app_valid_cancel_rollback`.
- [ ] Rolls back with `esp_ota_mark_app_invalid_rollback_and_reboot` if it isn't good within **5 minutes**, measured by the monotonic timer from the start of the trial boot. An answer at 300 seconds or later is too late.
- [ ] **Until it is marked good:** no heartbeat, no firmware report, no other message naming the new version, and no acting on offers. A heartbeat naming the offered version tells the hub the update installed.
- [ ] **Once marked good, and only then:** raise the floor to the release's `security_counter` if higher, replace the trusted keys with its `key_set` if it had one, clear the pending parts of the update record, and report `installed`.

Any reset before it is marked good makes the bootloader go back to the previous image. That firmware, finding an update record it didn't finish, **drops the pending `key_set`**, leaves the floor alone, and reports `rolled_back`. Firmware that finds an attempt cut short mid-download (a power cut) reports `failed` with `download`.

### Reports

`POST /v1/device/firmware/report`, answered `204`, with `{"offer_id", "state", "reason", "firmware_version"}`. `firmware_version` is the version running when you send it.

| `state` | `reason` |
|---|---|
| `downloading` | none: sent once, as the image starts arriving |
| `installed` | none |
| `refused` | `no_trusted_keys`, `too_large`, `bad_format`, `untrusted_key`, `bad_signature`, `wrong_device_type`, `wrong_hardware`, `downgrade` |
| `failed` | `download`, `image_hash`, `flash` |
| `rolled_back` | none |

Send a waiting final report after a poll the hub answered, before acting on anything new, and never in a loop. While one waits, act on no new offer. It waits across restarts, except a refusal made from the offer alone (`no_trusted_keys`, `too_large`), which is forgotten at a restart like the refusal itself.

Which answers settle it is the display's choice. The reference display counts any answer at all, even `not_found`. The official firmware counts only these, because the protocol's error table makes most other codes faults to retry or transient:

- any `2xx` (the hub answers `204` to a report for an offer already settled);
- `404` with `not_found`: the hub has no such offer;
- `token_revoked` or `unknown_device` with a 4xx status: the hub no longer knows the display.

Anything else leaves it waiting: no answer, a `5xx`, `rate_limited`, `unavailable`, a fault such as `bad_request` or `conflict`, a code it doesn't know, or an answer with no error body. So that a hub that always refuses can't hold updates back for good, it gives a report up once the hub has refused it 10 times since the display started. Only a lasting refusal counts: a 4xx answer with a fault code, an unknown code or no error body. A hub that is busy or away never counts (no answer, any 5xx, 408, 429, `rate_limited`, `unavailable`), so an outage of any length costs no report. A report that can't be written at all is given up at once.

## First install over USB, from the app

### What the app does

**Install firmware** in the desktop app (`apps/hub-manager/src/firmware.rs` and `flasher.rs`):

1. Checks the package in full. It must have `usb_install`.
2. Shows who signed it, and every key in its `key_set`, with fingerprints, before writing anything.
3. Connects to the chip's built-in loader through `espflash` at 115200 baud. It checks the chip is `usb_install.chip`, the flash is at least `min_flash_bytes`, and the chip isn't in Secure Download Mode.
4. **Erases the whole flash.**
5. Writes each USB part at its offset, in the manifest's order, then the app image at `app_offset`. All are written as raw bytes and read back.
6. Restarts the board once, at the end, and lets go of the port.
7. Opens the port again, with DTR on and RTS off, and sends a provisioning bundle carrying the trusted keys (the package's `key_set`, or else the key that signed it) and the package's security counter as the starting floor.

So after a first install the display starts with blank NVS, a full attempt budget, and the keys and floor from that bundle.

### The partition layout

The app writes only what the package lists, after erasing everything. An over-the-air update only ever writes the spare app partition, never the bootloader or the partition table. **So the layout a first install sets is the layout for good:** changing it later needs another **Install firmware**, which erases the keys too.

**Required:**

- An `otadata` partition and two OTA app partitions, `ota_0` and `ota_1`. Updates and rollback need both.
- `app_offset` is where the bootloader looks first on a blank OTA data partition: the `factory` partition if there is one, otherwise `ota_0`.
- Each OTA partition is at least as big as any image you'll ship. The format allows images up to 8 MiB. Make both the same size, since either can be the spare one. Report the spare one's size as `slot_bytes`.
- Every part and the app lie inside `min_flash_bytes`, at offsets that are multiples of 4096, with no overlaps. ESP-IDF itself wants app partitions at multiples of 64 KiB (0x10000).
- At most 4 USB parts: normally the bootloader, the partition table and the OTA data.
- An NVS partition big enough for what you keep (see [Storage and NVS](#storage-and-nvs)).

**What the test image used, which isn't a requirement.** The app's flasher was tried on a V2 board with a throwaway ESP-IDF test image, using ESP-IDF's two-OTA layout on 4 MB:

| Part | Offset |
|---|---|
| Bootloader | 0x0 |
| Partition table | 0x8000 |
| NVS | 0x9000 (16 KiB) |
| OTA data (a package part) | 0xd000 |
| `factory` (where the app was written) | 0x10000 |
| `ota_0`, `ota_1` | 1 MB each |

It erased, wrote all four writes, restarted and booted. That layout has two flaws for real firmware: 1 MB slots are small for an LVGL firmware, and its 16 KiB NVS can't hold a 32 KiB snapshot. A `factory` partition is allowed, but an over-the-air update never writes it, so it holds the first install's image for good.

The official firmware's layout for the 16 MB board (`firmware/partitions.csv`), tried on real V2 boards. It has no `factory` partition, so `app_offset` is `0x20000` and a first install boots from `ota_0`:

```text
# Name,    Type, SubType, Offset,   Size
nvs,       data, nvs,     0x9000,   0x10000
otadata,   data, ota,     0x19000,  0x2000
phy_init,  data, phy,     0x1b000,  0x1000
ota_0,     app,  ota_0,   0x20000,  0x600000
ota_1,     app,  ota_1,   0x620000, 0x600000
nvs_setup, data, nvs,     0xc20000, 0x20000
```

That gives two 6 MiB slots, a 64 KiB NVS for what a factory reset keeps, a 128 KiB NVS of its own for what a factory reset erases ([Factory reset](#factory-reset) says why it is a partition), and about 3.75 MiB left at the end for anything else. (The boards were tried with the first five lines; `nvs_setup` was added after them, at the end so that nothing else moved. Boards installed before it must be reinstalled over USB once: an over-the-air update can't change the partition table, so firmware that expects `nvs_setup` fails its trial boot on the older layout and is rolled back, using up an update attempt each time. Settle your own layout before your first release for the same reason.) On the ESP32-S3 the bootloader is at 0x0 and the partition table at 0x8000 by default; other chips differ (the original ESP32's bootloader is at 0x1000). Take the offsets from your own build's `build/flasher_args.json`, not from here.

### Packaging and signing with `hub-firmware`

Build the tool with `cargo build --release -p hub-firmware`. Make two keys once: one for signing, one for recovery, kept offline. **Publish both fingerprints**: owners compare them at a first install.

```sh
hub-firmware keygen --out my-displays.dhkey
hub-firmware keygen --out my-displays-recovery.dhkey
```

Each key file is protected by a passphrase of at least 12 characters, asked for on the terminal. Never commit a key file.

Sign each release. Name both keys in `--key-set` from the first release, so every display trusts both:

```sh
hub-firmware sign --key my-displays.dhkey \
  --image build/my-display.bin \
  --device-type my-display --hardware my-board-v1 --hardware my-board-v2 \
  --version 1.0.0 --security-counter 0 \
  --summary "First release." \
  --key-set my-displays.dhkey --key-set my-displays-recovery.dhkey \
  --usb-part bootloader@0x0=build/bootloader/bootloader.bin \
  --usb-part partition-table@0x8000=build/partition_table/partition-table.bin \
  --usb-part otadata@0x19000=build/ota_data_initial.bin \
  --app-offset 0x20000 --chip esp32s3 --min-flash 0x1000000 \
  --out my-display-1.0.0.dhfw
hub-firmware inspect my-display-1.0.0.dhfw
```

- `--key-set` takes a key file (it reads only the public half) or a file holding the 130 hex digits of a public key.
- `--usb-part`, `--app-offset`, `--chip` and `--min-flash` go together; leave all four out for a package that is only for over-the-air updates.
- `sign` warns when the image doesn't look like an ESP-IDF app image, or when the version in its `esp_app_desc_t` differs from `--version`. Set the project version in your build to match.
- Raise `--security-counter` in any release that fixes a security hole, and in any release whose `--key-set` drops a key.
- `inspect` needs no key. It prints the manifest, the signer's fingerprint and the package id.
- `hub-firmware login`, `add` and `publish` upload to a hub as a publisher; see [firmware-updates.md](firmware-updates.md), "The signing tool".

## Lessons from a real ESP32-S3 board

These were seen on a V2 board running a throwaway ESP-IDF 5.5 test image, which printed a line every few seconds and spoke no protocol:

- **DTR.** On this board's USB-Serial-JTAG, console output only flows once the host asserts DTR. With DTR off, the host reads nothing.
- **Opening the port may reset the chip.** Opening it with DTR asserted can reset the chip: the boot log then shows `rst:0x15 (USB_UART_CHIP_RESET)`. It doesn't happen every time (one test with the official firmware saw no reset, a later one did), so be ready either way. When it does, the first `hello` is lost while the board boots. That is why the app opens the port with DTR on and RTS off and repeats `hello` every 2 seconds. Firmware must be ready for a `hello` soon after start-up, and answer every one.
- **The factory slot.** Calling `esp_ota_mark_app_valid_cancel_rollback` while running from a `factory` partition logs `E esp_ota_ops: Running firmware is factory`. It is harmless.
- **Read the port.** Firmware that never reads the port makes the app's setup write time out (Windows `os error 121`).
- **Download mode.** There is no RESET button. The app tries to reset the board into its loader by itself; if it can't connect, hold BOOT while plugging in.
- **The flasher resets once.** The app stays in the loader between parts and resets once at the end, after the last write. An earlier version that reset after every part stopped after the bootloader.

Since then, the official firmware has passed its tests on real V2 boards, covering the screen and touch, the partition layout, setup over USB with its refusals, encrypted NVS, factory reset, Wi-Fi, enrolment, pinned TLS, polling, heartbeats, the saved snapshot, and over-the-air updates with the trial boot, including going back at the 5-minute limit and after a crash. One more lesson came from that:

- **Wi-Fi: 2.4 GHz only, and boards vary.** The ESP32-S3 has no 5 GHz radio, so the network must offer 2.4 GHz. One board found its network in every scan but never joined it: each join failed with reason 2 (`AUTH_EXPIRE`), even with Espressif's plain station example, while other boards joined the same network. A board that scans fine but never joins may have a faulty radio: try another board before changing the firmware or the network.

## Storage and NVS

**Decided 18/11/2026.** The official firmware uses **NVS encryption with the HMAC-based key protection scheme**, and keeps **flash encryption and Secure Boot off**. [firmware-updates.md](firmware-updates.md), "[Secure Boot, flash encryption and eFuse anti-rollback](firmware-updates.md#secure-boot-flash-encryption-and-efuse-anti-rollback)", says why those two stay off. Makers' own firmware SHOULD do the same.

| Item | Where | Notes |
|---|---|---|
| Wi-Fi SSID and password | Encrypted NVS | Never logged or shown. |
| Hub hosts, port, trust anchors | Encrypted NVS | Each PEM is under 4 KiB. |
| Device ID and token | Encrypted NVS | The token is the display's only credential. Never logged or shown. |
| Trusted firmware keys and security floor | NVS, apart from the setup | Public, but kept apart so a factory reset can leave them: the official firmware keeps these and the update record in one NVS partition and everything a reset erases in another ([Factory reset](#factory-reset)). |
| Update record, with the attempt budget | NVS | Small. Written a few times an hour at most, while the budget refills. |
| Last good snapshot | NVS blob, or a small file system | At most 32 KiB. Save it only when it changes. |

The enrolment secret is never stored past enrolment, and no provider credential is ever sent to a display.

**The settings:**

- `CONFIG_NVS_ENCRYPTION` on.
- `CONFIG_NVS_SEC_KEY_PROTECT_USING_HMAC` on.
- `CONFIG_NVS_SEC_HMAC_EFUSE_KEY_ID` set to the eFuse key block to use, 0 to 5. Its default is -1, so **it must be set**. Choose one block and never change it between releases: a different block means a different key, and the NVS written with the old one can't be read.

**What Espressif's documentation says** ([ESP-IDF v5.5, ESP32-S3: NVS encryption](https://docs.espressif.com/projects/esp-idf/en/v5.5/esp32s3/api-reference/storage/nvs_encryption.html) and [HMAC](https://docs.espressif.com/projects/esp-idf/en/v5.5/esp32s3/api-reference/peripherals/hmac.html)):

- the HMAC scheme works "without enabling flash encryption";
- the encryption keys are derived at run time, so there is no `nvs_keys` partition;
- if the chosen eFuse block is empty, `nvs_flash_init()` generates a key and burns it there with the purpose `HMAC_UP`. **That is permanent:** it uses up that key block for good;
- it is an error if the block already holds a key with another purpose.

**What it protects, honestly:**

- A plain read of the flash no longer shows the Wi-Fi password or the device token.
- Someone with a cable who runs their own firmware on the chip can most likely still use its HMAC unit to decrypt. That is physical access, which this project doesn't defend against.
- A stolen display's token can be revoked in the app, as before.

**Checked on a real board.** Espressif's pages don't say either of these, so both were tried on a V2 board with the official firmware, and both passed:

1. the burnt HMAC key doesn't stop normal reflashing and erasing over USB: after the burn, the app's **Install firmware** erased and reflashed the board, and the firmware started cleanly, as after any erase;
2. the generated key is read- and write-protected: it reads back as all zeros, so it can't be read out of the eFuses, and it can't be changed.

So NVS encryption stays on. Flash encryption and Secure Boot stay off. A maker whose own checks, on another chip or ESP-IDF release, fail should ship **plain NVS** instead and say so plainly in the firmware's documentation.

ESP-IDF's `nvs_flash_init()` burns a key into an empty block whatever state the chip is in. The official firmware never calls it: it checks the eFuses itself, and starts NVS with `nvs_flash_secure_init_partition()`, which can't burn anything ([firmware/README.md](../firmware/README.md#the-storage-key)).

**When to burn the key.** Not at start-up. ESP-IDF burns the key block first and its purpose and protection bits second, and a reset between the two leaves a key block that is neither empty nor usable: the board can then never be set up. The moments after an install are when a reset is most likely (the app opening the USB port can reset the chip). The official firmware leaves a new board's key unburnt, answers `hello` as a display with nothing stored (which is true), and burns the key when the first `provision` has passed every check, just before its first write: the port is open then, and the app is waiting for the answer. A key block found in that half-finished state is reported and never adopted, because it can't be told from a key someone else loaded and kept a copy of.

**Check the key you find.** Use an existing key only if it is read-protected, write-protected and has its purpose locked, as ESP-IDF's own key generation leaves it. A key with the right purpose that can still be read out protects nothing.

**Size the NVS partition.** ESP-IDF limits a blob to 97.6% of the partition size minus 4000 bytes, so a 32 KiB snapshot needs a partition of about 37 KiB at the very least, before anything else is stored. ESP-IDF's ready-made two-OTA table has only 16 KiB. The example layout above keeps it in a 128 KiB partition, with the rest of what a factory reset erases.

**Replacing a large blob needs room for two.** NVS writes a blob's new value before it lets go of the old one. In a 64 KiB partition that also holds the setup, replacing one 32 KiB snapshot with another in a single `nvs_set_blob()` fails with `ESP_ERR_NVS_NOT_ENOUGH_SPACE`, leaving the old one whole (tried with ESP-IDF v5.5.6's NVS code). In the official firmware's 128 KiB setup partition it succeeds every time (tried the same way, 1500 saves beside the largest setup), so the old snapshot is kept until the new one is whole. With less room, erase the old one first and accept that a power cut between the two leaves none.

**Don't save every snapshot.** `generated_at` moves with every poll, so "only when it has changed" must leave that field out of the comparison, and a page's `updated_at` still moves whenever the hub collects. At the shortest refresh (30 seconds), a 32 KiB write every poll would use up the rated erase cycles of a small partition (about 100,000 for each sector) within months. The official firmware replaces the saved copy at most once an hour.

**Keep the Wi-Fi driver out of NVS.** ESP-IDF's Wi-Fi driver stores the network's name and password in a namespace of its own by default, which a factory reset that erases only your own setup would leave behind. Turn that off (`CONFIG_ESP_WIFI_NVS_ENABLED`, and `esp_wifi_set_storage(WIFI_STORAGE_RAM)`) and hand the driver the details from your own storage at each start.

## Factory reset

A deliberate action on the device itself, such as holding a button during start-up: your choice, but document it. The official firmware's is holding **BOOT** for 10 seconds while it runs, with a countdown on screen ([firmware/README.md](../firmware/README.md#factory-reset)).

- **It erases:** the Wi-Fi settings, the hub hosts, port and trust anchors, the device ID and token, any stored bundle, and the saved snapshot. Then the display goes back to waiting for setup.
- **It keeps:** the trusted firmware keys, the security floor and the update record, with its attempt budget. Those belong to the firmware's maker, not to the hub the display was paired with. So never erase the partition they are in.
- **Erase the flash, not just the entries.** `nvs_erase_key()` and `nvs_erase_all()` only mark entries as erased: the bytes stay in flash until NVS reclaims the page, which may be never. With NVS encryption but no flash encryption, someone holding the board can run their own program on the chip and have its HMAC unit work the NVS keys out, so the "erased" Wi-Fi password and token could still be read. The official firmware keeps everything a reset must remove in an NVS partition of its own (`nvs_setup`), and a reset stops NVS on it (`nvs_flash_deinit_partition`), erases every sector (`esp_partition_erase_range`), reads each back as 0xFF, and starts NVS on it again. A mark written in the other partition before the erase, and taken out after it, makes a start after a power cut finish the job before reading anything (and if the mark can't be written, the reset doesn't begin); without it NVS starts on a half-erased partition without complaint, and the rest of the old setup stays in flash.
- A display must never accept a new bundle without a factory reset, so someone with a cable can't silently point it at another hub. A factory reset followed by a cable can point it at another hub, but can't change whose firmware it accepts (`firmware_keys_locked`).
- A maker MAY add a physical button action that refills the attempt budget, as a USB bundle does.

## TLS, exactly

As in `docs/protocol-v1.md`, "TLS and the hub's identity":

- `esp_http_client_config_t.cert_pem` is the bundle's `certificate_pem`, the only trust anchor. Never use a certificate bundle of public authorities. With two anchors, try both.
- `common_name = "hub.display-hub.internal"`, with `skip_cert_common_name_check = false`, whatever address you connect to.
- Allow TLS 1.2 and 1.3. The hub's key is ECDSA P-256, and it prefers AES-256-GCM.
- `CONFIG_MBEDTLS_SSL_IN_CONTENT_LEN=4096` is enough: the hub's records are at most 4 KiB.
- The certificate is valid from 1970 to 9999, so a display that doesn't know the date still accepts it.
- The firmware download uses exactly the same settings.
- Bound how long a request can take, as well as how big its answer is, **the TLS handshake included**: anything that answers at a hub address takes part in the handshake, with or without the hub's key, and a blocking handshake whose only limit is a timeout on each read can be held open for hours a byte at a time. ESP-IDF's `esp_http_client` can't keep such a limit by itself, nor be stopped part-way, on its built-in connection. The official firmware gives each request 30 seconds from its start for everything (the connection, the handshake, the request, and the answer's headers and body) and hands the client a connection of its own, on a socket that never blocks, which looks at that limit and at a factory reset before every read and write ([firmware/README.md](../firmware/README.md#talking-to-the-hub), "Limits"). A download's body has its own limits ([Acting on an offer](#acting-on-an-offer)).

`tools/mbedtls-interop/client.c` shows these settings in plain mbedTLS. Its checks pass against the real hub with mbedTLS 3.6: TLS 1.2 and 1.3 work, and a different hub key or a wrong name is refused. ESP-IDF 5.5 ships Mbed TLS 3.6. The current stable ESP-IDF (6.x) ships Mbed TLS 4, which this project hasn't tested, so pin an ESP-IDF 5.5 release unless you check 6.x yourself.

## Testing without the hardware

From a checkout of this repository, with Rust installed (see `docs/developer-setup.md`):

1. **Unit tests, on the host:** port `protocol/v1/vectors` (57) and `protocol/v1/firmware` (79), and parse every example. Keep the protocol core in plain C (or whatever you use) with no hardware, so these run in CI.
2. **Check the reference still agrees:** `cargo run -p display-simulator -- conform check` runs every vector through the reference display.
3. **A real hub on a PC:**
   - `cargo run -p hubd -- serve --data-dir dev-data/hub --listen 0.0.0.0:8443`
   - The board and the PC must share a network: the hub accepts only local addresses.
   - Make a setup file with `cargo run -p hubd -- display-bundle --data-dir dev-data/hub --name Desk --host <the PC's address> --out bundle.json`, or use the desktop app's **Displays**, **Add a display**, which sends the bundle over USB.
   - `hubd dev` (plain HTTP, sample figures, no authentication) is for the simulator only. Firmware never talks to it.
4. **USB setup:** the desktop app sends the bundle to the board's port. To watch the app's side first, `display-simulator usb --state <folder>` answers the same exchange on a pseudo-terminal (Linux only: on macOS the app can't open a pseudo-terminal as a serial port).
5. **Side by side:** `display-simulator enrol --bundle bundle.json --state <folder>`, then `display-simulator poll --state <folder>`, with a second bundle, shows what the board should show.
6. **Updates:** sign two versions with `hub-firmware`, upload them in the app, and press **Update**. `display-simulator firmware show --state <folder>` prints the simulator's keys, floor, slots and update record, and `display-simulator firmware set` sets test switches (a trial boot that never reaches the hub, a power cut part-way through) to try the failures by hand.

## A prompt for your own firmware

A maker can give this to their own coding agent, in their own repository. Fill in the parts in angle brackets.

````text
Build firmware for <my display: board, chip, screen and touch controllers> that implements the
Display Hub display protocol v1, with signed firmware updates. Work in this repository.

Sources, read-only: clone https://github.com/penguin97040/display-hub to a temporary folder at
commit <commit>, and copy protocol/v1 (schemas, examples, snapshot vectors and firmware vectors)
into this repository, recording that commit. Don't change the display-hub repository.
- docs/protocol-v1.md is the contract, docs/firmware-updates.md is the signed-update design, and
  docs/firmware-handoff.md is the brief. Follow all three; where they differ from the code, the
  code is the truth.
- tools/mbedtls-interop/client.c shows the TLS settings. tools/display-simulator is the reference
  display: when unsure what a display does, read it.
- firmware/ is the official firmware (ESP-IDF, with its protocol core in plain C in
  firmware/components/dh_core): a worked example of all of this. Copy from it where it fits.

My choices: device_type "<my-device-type>" (it must never change once released), hardware
revisions <list>, and factory reset by <button action>.

Plan first, then work in milestones, and commit as you go:
1. Toolchain: a pinned ESP-IDF 5.5 release, drivers for my board, and a graphics library. Build in
   CI. Unit tests run on the host.
2. Protocol core with no hardware: the lenient snapshot parser with the 32 KiB limit, pages and
   the "can't show this" card, ages and countdowns on the hub's clock, error groups and back-off.
   Every snapshot vector and every example passes as a host test.
3. Signed-update core with no hardware: the package checks in order (signature verified before
   the manifest is parsed), the attempt budget, the trial-boot rules and the bundle key rules.
   Every firmware vector passes as a host test.
4. USB setup (hello, provision, the refusal order in the brief), storage in NVS encrypted with the
   HMAC scheme (with the two board checks in the brief, and plain NVS if they fail), and factory
   reset keeping the firmware keys.
5. Wi-Fi, enrolment, pinned HTTPS (cert_pem plus common_name hub.display-hub.internal; no CA
   bundle; TLS 1.2 and 1.3), polling, heartbeat, and the last good snapshot in flash.
6. Over-the-air updates: the partition layout in the brief, app rollback, the download into the
   spare partition, the trial boot and the reports. A script that signs a release with
   hub-firmware, with the USB-install parts.
7. Screens: one per page in the hub's order, swiping, data-quality labels, "unknown" (never 0),
   stale and "reset due", the unavailable note, and plain-language problems.

Never log or show the device token, enrolment secret or Wi-Fi password. Never skip certificate
verification. Never store provider credentials, and never commit a signing key. Say plainly what
you tested on real hardware and what you didn't, and list what I must check on the board.
````

## What each firmware decides

These are the firmware's to choose and document. The official firmware's choices are in brackets.

- How it lays out pages, beyond the rules above (one page at a time in the hub's order, then a status screen: [firmware/README.md](../firmware/README.md#the-screen)).
- Which button does a factory reset (**BOOT**, held for 10 seconds while running).
- Whether to use SNTP (no: ages are worked out on the hub's clock).
- How to tell V1 from V2 at run time, or whether to build one image for each (by which touch chip answers; only V2 is supported, and a V1 board says so on its screen).
- Its `device_type`. Packages are matched to it byte for byte, so once released it never changes (`display-hub-waveshare-amoled-1.8`, with the hardware revision `waveshare-amoled-1.8-v2`).
- The exact partition layout and slot sizes, within the rules above (two 6 MiB slots and a 64 KiB NVS: [The partition layout](#the-partition-layout)).
- Which eFuse key block the NVS key uses, and whether NVS encryption is on (KEY5, and on).
- Which custom kinds it draws, if any (none yet: every custom kind gets the card. `dh_core`'s snapshot reader takes a `custom_kind_supported` function for firmware that draws its own).
