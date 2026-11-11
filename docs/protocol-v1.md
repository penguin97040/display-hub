# Display protocol, version 1

**Status: frozen on 11/11/2026, at the end of milestone 2.** From now on, v1 changes only by adding things (see Compatibility). Everything here is implemented in `hubd` and tested, including against mbedTLS 3.6, the line ESP-IDF 5.x uses.

This document is for anyone writing display firmware. The machine-readable parts are in `protocol/v1/`: JSON Schemas for every message and examples for each. The Rust reference implementation is `crates/hub-protocol`, and `tools/display-simulator` is the reference display.

## Transport

- The display makes outbound HTTPS requests to the hub on the local network, on port 8443 by default. It never listens on a port.
- JSON bodies are UTF-8. Every request carries `X-Display-Protocol: 1`, and every authenticated request carries `Authorization: Bearer <device token>`.
- **Development only:** `hubd dev` serves the same snapshot over plain HTTP on `127.0.0.1`, with no authentication. Firmware never talks to it.

### TLS and the hub's identity

The hub has one self-signed certificate. A display trusts that certificate and nothing else:

| Setting | Value | ESP-IDF (`esp_http_client_config_t`) |
|---|---|---|
| Trust anchor | `certificate_pem` from the provisioning bundle. With two anchors during a key change, either one. | `cert_pem` |
| Name to check | Always `hub.display-hub.internal`, whatever address you connect to | `common_name` |
| Verification | Required. Never skip it. | `skip_cert_common_name_check = false` |
| Versions | TLS 1.2 (required) and TLS 1.3 | Default |
| Key and suites | ECDSA P-256. ECDHE-ECDSA with AES-GCM or ChaCha20; the hub prefers AES-256-GCM, which ESP32 accelerates. | Default |
| Record size | The hub sends records of at most 4 KiB | `CONFIG_MBEDTLS_SSL_IN_CONTENT_LEN=4096` is enough |
| Validity | 1970 to 9999-12-31, so a display that doesn't know the date still accepts it | |

Never use a certificate bundle of public authorities, and never trust an address or `.local` name on its own: mDNS answers are not proof of identity. For firmware that pins keys instead, `spki_sha256` is the SHA-256 of the certificate's SubjectPublicKeyInfo.

`tools/mbedtls-interop` checks these settings with a real mbedTLS client:

- TLS 1.2 and 1.3 connect;
- a different hub's key is refused;
- the wrong name is refused.

## `POST /v1/device/enrol`

This is sent once, after provisioning. It needs no bearer token: the body carries the bundle's one-time secret.

```json
{ "enrolment_secret": "q3H7cX0f2pS9L1mB8vN4wA",
  "device": { "device_type": "llm-usage-display", "firmware_version": "1.0.0",
              "hardware_revision": "waveshare-amoled-1.8-v2", "protocol_version": 1 } }
```

- **Success:** `201` with `{"device_id": "d-3fa91c07b2e4", "device_token": "dhd_…"}`. Store the token in encrypted NVS. It is the display's only credential. Never log or display it.
- **`enrolment_used` (409):** the code was already used. The hub records this as a security alarm.
- **`enrolment_expired` (401):** the code is past its 10 minutes, or never existed.
- **`rate_limited` (429):** too many attempts from this address.

Schemas: `enrol-request.schema.json` and `enrol-response.schema.json`.

## `POST /v1/device/heartbeat`

This is optional and authenticated. Its body has the same four `device` fields as enrolment (`heartbeat.schema.json`). The hub records at most one a minute, keeping only these fields and the time last seen, which the desktop manager shows. Send one after start-up and then hourly. The answer is `204`.

## `GET /v1/health`

No authentication. It returns `{"status": "ok", "protocol_versions": [1]}` and nothing else, never account or credential details.

## `GET /v1/device/snapshot`

Returns the display's dashboard. It requires the display's token, and the hub takes the display's identity from that token only, never from anything the request names. Without a valid token the answer is `401` with `unauthorised`, or `token_revoked` once the display has been removed in the manager.

```json
{
  "protocol_version": 1,
  "message_type": "dashboard.snapshot",
  "dashboard_id": "llm-usage",
  "generated_at": "2026-11-10T20:00:00Z",
  "refresh_after_seconds": 300,
  "pages": [
    {
      "id": "claude-max",
      "title": "Claude Max",
      "data_quality": "provider_reported",
      "updated_at": "2026-11-10T19:55:00Z",
      "stale_after_seconds": 1800,
      "note": null,
      "metrics": [
        { "id": "five-hour", "label": "Five-hour allowance used", "used_percent": 42.0, "resets_at": "2026-11-10T22:00:00Z" },
        { "id": "weekly", "label": "Weekly allowance used", "used_percent": null, "resets_at": null }
      ]
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `generated_at` | When the hub built the snapshot, by the hub's clock. All ages are measured from it. |
| `refresh_after_seconds` | How long to wait before the next poll (30–3600). Add jitter of ±10%. |
| `pages` | Exactly the pages to show, in order. There are at most 10. An empty list means "no pages yet". |
| `page.id` | Stable across snapshots, so the display can keep its place when pages move. It uses `a-z`, `0-9` and `-`, up to 64 characters. |
| `page.title` | Plain text, 1–48 characters. |
| `page.data_quality` | Where the figures came from. See below. |
| `page.updated_at` | When the figures were collected. It is `null` only for unavailable pages. |
| `page.stale_after_seconds` | The figures are stale once they are older than this (60–604800). |
| `page.note` | Optional plain text, up to 96 characters. For example, why a page is unavailable. Show it. |
| `metric.used_percent` | Share of the allowance **used** (not remaining), from 0 to 100. `null` means unknown. **Never draw `null` as zero.** |
| `metric.resets_at` | When the window resets, if known. |

### Timestamps

Timestamps are always `YYYY-MM-DDTHH:MM:SSZ`: UTC, exactly 20 characters, with no fractions or offsets. A fixed-width scan parses them.

### Data quality

| Value | Show as | Meaning |
|---|---|---|
| `demo` | DEMO | Sample figures for trying the system. Never real usage. |
| `provider_reported` | REPORTED | Reported by the provider through a documented interface. |
| `estimated` | ESTIMATE | Worked out locally. Approximate. |
| `manual` | MANUAL | Typed in by the owner. |
| `unavailable` | UNAVAILABLE | No figures. Show the `note`. Such a page never has metrics. |

These are not interchangeable, and the display must always show which one applies. Any value the display doesn't recognise must be treated as `unavailable`.

### Age and staleness without a clock

A display may not know the time. Measure everything on the hub's clock:

```text
age          = (generated_at − updated_at) + (time since the snapshot arrived, by a monotonic timer)
stale        = age > stale_after_seconds
reset in     = resets_at − (generated_at + time since the snapshot arrived)
```

Once a reset time has passed, show "reset due" instead of a countdown. The hub clears a figure when its window has reset (the share becomes `null`, with a note), so the next snapshot corrects it.

### Size and bounds

A snapshot is never larger than **32 KiB**, and per-field bounds keep even the worst case (four-byte characters everywhere) under that limit. The tests check this. Text is Unicode NFC, without control or invisible formatting characters and without leading or trailing spaces.

## Errors

Errors are JSON: `{"error": {"code": "...", "message": "..."}}`. The `message` is plain English for people. Act on the `code`:

| Codes | Display action |
|---|---|
| `token_revoked`, `unknown_device`, `enrolment_used`, `enrolment_expired` | Stop polling (retry hourly). Show that the display needs setting up again from the desktop manager. |
| `incompatible_version` | Show that the hub and display software don't match. Retry hourly. |
| `rate_limited`, `unavailable`, and any code you don't recognise | Keep the last good data, and retry with backoff. |
| `unauthorised`, `forbidden`, `bad_request`, `payload_too_large`, `not_found`, `conflict` | Treat as a fault. Retry slowly, and log the code. |

## What a display does

- **Keep the last good snapshot** in flash, with the time it arrived. Show it after a restart, a failed poll or an outage, marked with its age, and with stale pages marked as stale.
- **Accept a response** only if:
  - it is at most 32 KiB, which you check before parsing;
  - `protocol_version` is 1 and `message_type` is `dashboard.snapshot`;
  - it follows the rules above.

  Otherwise keep the last good snapshot and say the hub sent something unreadable.
- **Back off on failure:** 15 s, 30 s, 60 s and so on, up to 15 minutes, with ±20% jitter. On success, wait `refresh_after_seconds` (±10%).
- **Explain problems in plain words:**
  - can't reach the hub;
  - the hub sent unreadable data;
  - the hub is busy;
  - setup is needed;
  - versions don't match.

  `tools/display-simulator` implements all of this and is the reference for expected behaviour.

## Provisioning bundle

The desktop manager gives a display everything it needs in one JSON document (`provisioning.schema.json`; example in `examples/device/provisioning-bundle.json`):

| Field | Meaning |
|---|---|
| `hub.hosts` | One to four addresses to try, in order. For example `raspberrypi.local`, then a fixed IP. |
| `hub.port` | 8443 unless the owner changed it. |
| `hub.server_name` | Always `hub.display-hub.internal`. |
| `hub.trust_anchors` | One or two `{certificate_pem, spki_sha256}`. |
| `enrolment.secret`, `enrolment.expires_at` | The one-time secret for `/v1/device/enrol`, valid for 10 minutes. |
| `wifi` | `{ssid, password}` or `null`. A 2.4 GHz network; the password is empty for an open network. |

The bundle is secret while it's valid. Keep the hub details and Wi-Fi settings, use the secret once, then discard it. **Never store provider credentials on a display.** None are ever sent.

### Over USB (serial)

The manager talks to the display over its USB serial port (CDC-ACM or USB-Serial-JTAG) at 115200 baud, using UTF-8 JSON, one object per line ending in `\n`, with each line at most 16 KiB. Lines that don't start with `{` are ignored by both sides, so boot logs on the same port are harmless.

| Step | Manager sends | Display answers |
|---|---|---|
| 1 | `{"type":"hello","protocol_version":1}` | `{"type":"hello","device_type":"llm-usage-display","firmware_version":"…","hardware_revision":"…","protocol_version":1,"provisioned":false}` |
| 2 | `{"type":"provision","bundle":{…}}` | `{"type":"provisioned"}`, or `{"type":"error","code":"invalid_bundle"\|"already_provisioned"\|"storage_failed","message":"…"}` |

Rules:

- Each answer must arrive within 10 seconds.
- The manager repeats `hello` every 2 seconds until it gets an answer, in case the board restarted when the port opened. Answer every `hello`.
- The manager opens the port with DTR set and RTS clear, which doesn't reset an ESP32 through its USB serial bridge.
- A display that is already provisioned answers `already_provisioned`, and keeps its settings, until the owner factory-resets it on the device itself. Someone who plugs a cable in can't silently point it at another hub.
- A factory reset erases the Wi-Fi settings, trust anchors and token.
- After `provisioned`, the display joins the Wi-Fi network and enrols. The manager then watches for it to appear in the hub's list of displays.

`tools/display-simulator` stands in for the display at both stages: it enrols from a bundle file, and answers the serial exchange on a pseudo-terminal.

## Compatibility

- Displays **ignore fields they don't know**, and treat unknown `data_quality` values as `unavailable` and unknown error codes as transient. A hub may therefore add optional fields and new quality values within v1.
- A hub never removes or renames a field within v1, never changes a field's meaning, and never loosens a bound. The published schema stays strict, describing exactly what a v1 hub sends.
- A breaking change needs `protocol_version: 2`. A hub serves every version from `MIN_SUPPORTED_PROTOCOL_VERSION` (currently 1) up to its own. A display that sends a version the hub doesn't serve gets `incompatible_version`.

## Conformance

- `protocol/v1/examples/valid/*.json` must be accepted and drawn.
- `protocol/v1/examples/invalid/*.json` must be refused, apart from these, which a display tolerates (a hub never sends them):
  - `extra-field` is accepted, because unknown fields are ignored;
  - `unknown-quality` and `missing-used-percent` are accepted, read as unavailable and unknown respectively.
- `crates/hub-protocol/tests/conformance.rs` shows the exact expectation for each file.
