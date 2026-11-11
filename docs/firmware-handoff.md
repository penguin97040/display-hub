# Firmware handoff: `llm-usage-display`

The display firmware lives in its own repository, `penguin97040/llm-usage-display`, which is empty as of 11/11/2026. This repository defines what the firmware must do and supplies everything to test it against. It contains no firmware, and the firmware repository should not depend on this one as a submodule: copy the files below at a pinned commit instead.

## What to take from here

All of these are under `protocol/v1/` unless noted.

| What | Where | Use |
|---|---|---|
| The contract | `docs/protocol-v1.md` | The source of truth. Frozen on 11/11/2026; v1 only grows by optional additions. |
| JSON Schemas | `snapshot`, `enrol-request`, `enrol-response`, `heartbeat`, `error`, `provisioning`, `usb` (`*.schema.json`) | Check your structs and the messages you send. |
| Examples | `examples/valid`, `examples/invalid`, `examples/device`, `examples/usb` | Parser tests. Three invalid examples are tolerated by displays (see the protocol). |
| Conformance vectors | `vectors/` (40 cases, [format](../protocol/v1/vectors/README.md)) | Port to unit tests: response in, display state out. |
| TLS reference client | `tools/mbedtls-interop/client.c` | An mbedTLS 3.6 client configured as ESP-IDF's HTTP client should be, tested against the real hub. |
| Reference display | `tools/display-simulator` | Behaves as firmware must; run it beside a board to compare. |

Record the display-hub commit you copied from, and copy again only deliberately.

## The board

Waveshare **ESP32-S3-Touch-AMOLED-1.8**: [documentation](https://docs.waveshare.com/ESP32-S3-Touch-AMOLED-1.8), [examples](https://github.com/waveshareteam/ESP32-S3-Touch-AMOLED-1.8). Two hardware revisions exist, marked on the back of the board, with different display and touch controllers:

| Revision | Display controller | Touch controller | `hardware_revision` to report |
|---|---|---|---|
| V1 | SH8601 | FT3168 | `waveshare-amoled-1.8-v1` |
| V2 | CO5300 | CST820 | `waveshare-amoled-1.8-v2` |

- Support both in one firmware where you can, choosing the drivers at build time or by probing at start-up. Verify on each physical revision; neither has been tested from this repository.
- The display is 368 × 448 (the simulator draws at that size). Confirm it against Waveshare's documentation for your revision.
- The board runs on USB-C power. It talks to the hub over 2.4 GHz Wi-Fi, and uses USB only for setup.

## What the firmware does

### First start: waiting for setup

With no stored bundle, the display shows that it needs setting up from the desktop manager, and listens on its USB serial port, CDC-ACM or USB-Serial-JTAG at 115200 baud. The exchange is in `docs/protocol-v1.md`, "Over USB":

1. Answer every `hello` with a `hello` of your own, including `provisioned`. The manager repeats `hello` every 2 seconds until you answer, in case the board restarted when the port opened. The manager sets DTR and clears RTS when it opens the port.
2. On `provision`, take these steps in order:
   1. Refuse the bundle with `invalid_bundle` if it breaks a rule: check the same rules as `Bundle::problems()` in `crates/hub-protocol/src/provisioning.rs`.
   2. Refuse it with `already_provisioned` if you hold one already.
   3. Otherwise store it, and answer `provisioned`, or `storage_failed` if storing failed.
3. Ignore any line that doesn't start with `{`, and drop any line over 16 KiB. Boot logs on the same port are harmless.

### Joining and enrolling

1. Join the Wi-Fi network in the bundle. If its `wifi` is `null`, keep the existing settings.
2. `POST /v1/device/enrol` with the one-time secret and the four device fields. Try each `hub.hosts` entry in order; see "Finding the hub" in the protocol.
3. Store the returned `device_id` and `device_token`. **Erase the enrolment secret.** On `enrolment_used` or `enrolment_expired`, show that setup must be redone.

### Every poll

1. `GET /v1/device/snapshot`, with `Authorization: Bearer <device token>` and `X-Display-Protocol: 1`.
2. Refuse a body over 32 KiB before parsing it.
3. Parse leniently: ignore unknown fields, read an unknown `data_quality` as unavailable, and read a missing `used_percent` as unknown.
4. On success, draw exactly the pages given, in order. Keep the current page by `id` when pages move. Store the snapshot as the last good one.
5. Measure ages and countdowns on the hub's clock: `generated_at`, plus a monotonic timer since the response arrived. Never use the display's wall clock.
6. Act on errors by their group (`reprovision`, `incompatible`, `retry`, `fault`), keeping the last good data on screen. The vectors give the exact outcome for every case.
7. Wait `refresh_after_seconds` ±10% after a success. After a failure, back off from 15 s, doubling to 15 minutes, ±20%. For `reprovision` and `incompatible`, retry hourly.
8. Send `POST /v1/device/heartbeat` after start-up, then hourly.

### On screen

- Every page shows its data quality: DEMO, REPORTED, ESTIMATE, MANUAL or UNAVAILABLE.
- A `null` share reads "unknown", **never 0%**.
- A stale page says so, with its age.
- A passed reset reads "reset due".
- An unavailable page shows its `note` and no figures.
- Swiping left and right moves between pages.
- Problems use plain words:
  - can't reach the hub;
  - the hub sent unreadable data;
  - the hub is busy;
  - setup is needed;
  - versions don't match;
  - something else is answering as the hub.
- After a restart, show the saved snapshot. Its age is only known if the display has a trustworthy time since then, from SNTP for example. Otherwise mark it "saved; age unknown" until the next successful poll.

### Factory reset

A deliberate action on the device itself, such as holding a button during start-up: your choice, but document it. It erases the Wi-Fi settings, the trust anchors, the device token and the saved snapshot, then returns to waiting for setup. A display must never accept a new bundle without one, so someone with a cable can't silently point it at another hub.

## TLS, exactly

As in `docs/protocol-v1.md`, "TLS and the hub's identity":

- `esp_http_client_config_t.cert_pem` is the bundle's `certificate_pem`, the only trust anchor. Never use a certificate bundle of public authorities. With two anchors, try both.
- `common_name = "hub.display-hub.internal"`, with `skip_cert_common_name_check = false`, whatever address you connect to.
- Allow TLS 1.2 and 1.3. The hub prefers AES-256-GCM; ECDSA P-256.
- `CONFIG_MBEDTLS_SSL_IN_CONTENT_LEN=4096` is enough; the hub's records are at most 4 KiB.
- The certificate is valid from 1970 to 9999, so a display without the date still accepts it.

`tools/mbedtls-interop/client.c` shows these settings in plain mbedTLS. Its checks pass against the real hub: TLS 1.2 and 1.3 work, and a different hub key or a wrong name is refused.

## Storage

| Item | Where | Notes |
|---|---|---|
| Wi-Fi SSID and password | Encrypted NVS | Never logged. |
| Hub hosts, port, trust anchors | Encrypted NVS | Each PEM is under 4 KiB. |
| Device ID and token | Encrypted NVS | The token is the display's only credential. Never logged or shown. |
| Last good snapshot | Flash (NVS blob or a small filesystem) | At most 32 KiB. Save it only when it changes, to spare the flash. |

Enable NVS encryption with flash encryption, as ESP-IDF documents for your version. The enrolment secret is never stored past enrolment, and no provider credential is ever sent to a display.

## Testing without the Pi

1. **Unit tests:** port `protocol/v1/vectors`, and parse every example.
2. **A real hub on a PC:**
   - `cargo run -p hubd -- serve --data-dir dev-data/hub --listen 0.0.0.0:8443`.
   - The board and PC must share a network; the hub only accepts local addresses.
   - Make a setup file with `hubd display-bundle --name Desk --host <PC address> --out bundle.json`, or use the desktop manager.
3. **USB setup:** the desktop manager's **Displays**, **Add a display** sends the bundle to the board's port. The simulator answers the same exchange on a pseudo-terminal (`display-simulator usb`) if you want to watch the manager's side first.
4. **Side by side:** `display-simulator poll --state <dir>`, enrolled from another bundle, shows what the board should show.

## A prompt for the firmware session

Start a separate Claude Code session in a checkout of `llm-usage-display`, and paste this:

````text
Follow my agent-rules (penguin97040/agent-rules: read it, install its local notes, and follow its Git,
privacy and identity rules; this repository is public). Don't modify penguin97040/display-hub or
penguin97040/agent-rules.

Build ESP-IDF firmware for the Waveshare ESP32-S3-Touch-AMOLED-1.8, hardware revisions V1 (SH8601
display, FT3168 touch) and V2 (CO5300, CST820), that implements the Display Hub display protocol v1.

Sources, read-only: clone penguin97040/display-hub to a temporary folder at a pinned commit, and copy
protocol/v1 (schemas, examples, conformance vectors) into this repository with that commit recorded.
docs/protocol-v1.md is the contract and docs/firmware-handoff.md is the brief; follow both. Use
tools/mbedtls-interop/client.c for the TLS settings, and tools/display-simulator as the reference
display.

Plan first, then work in milestones:
1. Toolchain: a pinned ESP-IDF 5.x release, the board support package and drivers for both
   revisions, and LVGL. Build in CI. Unit tests run on the host.
2. Protocol core in plain C with no hardware: a lenient snapshot parser with the 32 KiB limit,
   age, staleness and countdown on the hub's clock, error groups, and backoff. All 40 conformance
   vectors and every example must pass as host tests.
3. USB setup over line JSON (hello, provision; refuse a second bundle), storage in encrypted NVS,
   and factory reset.
4. Wi-Fi, enrolment, pinned HTTPS (cert_pem plus common_name hub.display-hub.internal; no CA
   bundle; TLS 1.2 and 1.3), polling, heartbeat, and the last good snapshot in flash.
5. LVGL screens: one page per subscription in the hub's order, swiping, quality labels, "unknown"
   (never 0%), stale and "reset due", the unavailable note, and plain-language problems.

Never log or display the device token, enrolment secret or Wi-Fi password. Never skip certificate
verification. Never store provider credentials. Say plainly what you tested on which physical
revision, and list what the owner must check on the board.
````

## What is not decided here

- How the firmware lays out pages, beyond the rules above.
- Which button does a factory reset.
- Whether to use SNTP.
- How to tell V1 from V2 at run time.

Those are the firmware's to choose and document.
