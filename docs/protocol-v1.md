# Display protocol, version 1

**Status: reshaped on 16/11/2026 for general pages, and not yet frozen again.** Until it is, v1 may still change in ways that aren't just additions. Once it is frozen, it changes only by adding things (see Compatibility). Everything here is implemented in `hubd` and tested, including against mbedTLS 3.6, the line ESP-IDF 5.x uses.

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

Firmware that supports signed updates adds an optional `firmware` member to the `device` object, in enrolment, every heartbeat and the USB `hello`: its trusted key ids, security floor, spare partition size and, optionally, what is left of its attempt budget (see [Signed firmware updates](#signed-firmware-updates)). A hub that finds the member malformed ignores it and keeps the rest of the report. A heartbeat naming the version a display has been offered also tells the hub the update installed, if the display's own report was lost, unless a `rolled_back` or `failed` report for that offer arrives later. So new firmware on its trial boot sends no heartbeat until it is marked good.

## `GET /v1/health`

No authentication. It returns `{"status": "ok", "protocol_versions": [1]}` and nothing else, never account or credential details.

## `GET /v1/device/snapshot`

Returns the display's dashboard. It requires the display's token, and the hub takes the display's identity from that token only, never from anything the request names. Without a valid token the answer is `401` with `unauthorised`, or `token_revoked` once the display has been removed in the manager.

```json
{
  "protocol_version": 1,
  "message_type": "dashboard.snapshot",
  "generated_at": "2026-11-10T20:00:00Z",
  "refresh_after_seconds": 300,
  "pages": [
    {
      "id": "claude-max",
      "title": "Claude Max",
      "kind": "meter",
      "data_quality": "provider_reported",
      "updated_at": "2026-11-10T19:55:00Z",
      "stale_after_seconds": 1800,
      "note": null,
      "data": {
        "items": [
          { "id": "five-hour", "label": "Five-hour allowance used", "percent": 42.0, "resets_at": "2026-11-10T22:00:00Z" },
          { "id": "weekly", "label": "Weekly allowance used", "percent": null }
        ]
      }
    }
  ]
}
```

### The snapshot

| Field | Meaning |
|---|---|
| `generated_at` | When the hub built the snapshot, by the hub's clock. All ages are measured from it. |
| `refresh_after_seconds` | How long to wait before the next poll (30-3600). Add jitter of ±10%. |
| `pages` | Exactly the pages to show, in order. There are at most 10. An empty list means "no pages yet". Each display gets its own list: the pages ticked for it in the manager, in the hub's one shared order, up to the first 10. |
| `firmware_update` | Optional, and present only while the display has an open firmware offer: `{"offer_id", "package_id", "size"}`. A display that doesn't support signed updates ignores it; one that can't make sense of it reads it as absent. See [Signed firmware updates](#signed-firmware-updates). |

### A page

Every page has the same common fields, and a `kind` that says what is in its `data`.

| Field | Meaning |
|---|---|
| `id` | Stable across snapshots, so the display can keep its place when pages move. It uses `a-z`, `0-9` and `-`, up to 64 characters. Unique in the snapshot. |
| `title` | Plain text, 1-48 characters. |
| `kind` | What `data` holds: a ready-made shape or a custom kind. See below. |
| `data_quality` | Where the figures came from. See below. |
| `updated_at` | When the figures were collected. It is `null` only for unavailable pages. |
| `stale_after_seconds` | The figures are stale once they are older than this (60-604800). |
| `note` | Optional plain text, up to 96 characters. For example, why a page is unavailable. Show it. |
| `data` | The page's content, an object. **Absent (or `null`) when `data_quality` is `unavailable`, and required otherwise.** |

All text is plain: Unicode NFC, with no control or invisible formatting characters and no leading or trailing spaces, and never empty. Lengths count characters, not bytes.

A hub never sends a field that isn't listed here. A display ignores any it doesn't know, in the page and inside `data`. Where a field inside `data` is optional, omit it or use `null`: a display treats the two the same.

### Kinds

`kind` is either a **ready-made shape name** (`meter`, `big_number`, `text` or `list`) or a **custom kind**.

A custom kind looks like `nearby-flights/1`:

- a name in lower-case letters, digits and `-`, optionally in dot-separated parts (`org.example.board/2`);
- a `/`;
- a version number from 1 to 9999.

The pattern is `^[a-z0-9][a-z0-9-]*(\.[a-z0-9][a-z0-9-]*)*/[1-9][0-9]{0,3}$`, and a kind is at most 64 characters. **The version is part of the kind.** `nearby-flights/2` is a different kind from `nearby-flights/1`, so a display that only knows version 1 never misreads version 2 as version 1. Shape names never contain `/`, so a shape added in future can't clash with anyone's custom kind.

#### `meter`

One to six bars, each a share from 0 to 100.

```json
{ "items": [
  { "id": "five-hour", "label": "Five-hour allowance used", "percent": 42.0, "resets_at": "2026-11-10T22:00:00Z" },
  { "id": "weekly", "label": "Weekly allowance used", "percent": null }
] }
```

- `items`: 1-6, each with a unique `id` (same rules as a page `id`).
- `label`: up to 48 characters.
- `percent`: 0-100, or `null` for unknown. **Never draw `null` as zero**; say "unknown" or show a dash.
- `resets_at`: optional, when the item's window resets. Once it has passed, show "reset due" rather than a countdown.

#### `big_number`

One figure, drawn large.

```json
{ "value": 1234.5, "decimals": 1, "unit": "k", "caption": "Tokens today" }
```

- `value`: a number from -1e15 to 1e15 (`MAX_BIG_NUMBER_ABS`), or `null` for unknown. **Draw `null` as a dash, never as 0.** A value outside that range is bad data (a card).
- `decimals`: optional, 0-3, default 0. How many decimal places to draw. A whole number written as a float (`2.0`) counts.
- `unit`: optional, up to 8 characters.
- `caption`: optional, up to 48 characters.

#### `text`

A short paragraph.

```json
{ "text": "The monthly allowance resets on the first." }
```

- `text`: 1-280 characters.

#### `list`

Up to eight lines, each with an optional detail.

```json
{ "items": [ { "text": "display-hub", "detail": "passed" }, { "text": "firmware" } ] }
```

- `items`: 0-8, each with `text` (up to 48 characters) and an optional `detail` (up to 48 characters). An empty list is allowed, so a display shows "nothing listed".

#### Custom kinds

For anything the shapes don't cover, such as `nearby-flights/1`:

- `data` is any JSON **object**, with no more than 8 levels of nesting (the `data` object itself is the first level).
- The hub doesn't look inside it. What it means is agreed between whoever writes the hub-side source and whoever writes the firmware that draws it, and nobody else needs to know.
- A display whose firmware doesn't know the kind shows the "can't show this" card (below). That is the normal, safe outcome. A new custom kind never breaks a display that doesn't know it.
- Treat the version as a contract: change what `data` means, and you change the version.

### Size

- The whole snapshot is at most **32 KiB**, with a `firmware_update` member included.
- Each page, counted as its compact serialised JSON, is at most **3072 bytes** (`MAX_PAGE_BYTES`). Ten pages of that size, plus the envelope, still fit in 32 KiB.
- A full `list` with four-byte characters everywhere can be over 3072 bytes, so a hub must keep a long list within the page limit. The other ready-made shapes always fit.
- A hub never sends an over-size or invalid page. `hubd` checks every page it builds, and swaps one that breaks the page rules for an unavailable page with the same `id`, `kind`, `title` and `stale_after_seconds` and a note (for example "This page is too big to show"), so one bad page never stops the others being sent. The title is cleaned first, and a `kind` that is neither a shape nor a custom kind becomes `text`. An unavailable page carries no `data`, so the stand-in passes the same checks as any other page; one that still can't (a bad `id`, say) is left off.

### What a display does with a page it can't show

One bad page must not cost the display the rest of the snapshot. A display shows a **"can't show this" card** for a page when:

- its `kind` is a shape the display doesn't know, a custom kind it doesn't support, or doesn't read as a kind at all (**unknown kind**);
- its `data` breaks the rules for its shape: a value out of range, text too long or too many items, duplicate meter ids, a custom `data` that isn't an object or nests too deep, or `data` missing on a page that needs it (**bad data**);
- it has `data` and its `data_quality` is `unavailable` (**bad data**);
- it is over 3072 bytes (**too large**).

The card shows the page's `title`, then this text, exactly:

> This display can't show this page.

For an unknown kind only, a second line follows:

> It may need a firmware update.

The card shows nothing else about the page (no figures, note or age). The hub-protocol crate has these two lines as constants (`CANT_SHOW_TEXT`, `CANT_SHOW_UPDATE_HINT`), and the conformance vectors give the exact lines.

An unavailable page that has no `data` is not a card: show the title and `note`, as in the next section, whatever its `kind` says.

**Limits of the JSON parser.** A display may refuse a whole body as malformed if it is nested deeper than about 100 levels, or has a number outside the range of a double (such as `1e400`). A hub never sends either, so this is not a card case. Custom `data` nested within the limit above but deeper than 8 levels is a card.

Problems with the snapshot itself are different, and **do** make the display refuse it and keep the last good one: a wrong version or message type, a bad `refresh_after_seconds`, more than 10 pages, a body over 32 KiB, or a page whose common fields (`id`, `title`, `updated_at`, `stale_after_seconds`, `note`) break the rules or whose `id` repeats.

### Timestamps

Timestamps are always `YYYY-MM-DDTHH:MM:SSZ`: UTC, exactly 20 characters, with no fractions or offsets. A fixed-width scan parses them.

### Data quality

| Value | Show as | Meaning |
|---|---|---|
| `demo` | DEMO | Sample figures for trying the system. Never real usage. |
| `provider_reported` | REPORTED | Reported by the provider through a documented interface. |
| `estimated` | ESTIMATE | Worked out locally. Approximate. |
| `manual` | MANUAL | Typed in by the owner. |
| `unavailable` | UNAVAILABLE | No figures. Show the `note`. Such a page has no `data`. |

These are not interchangeable, and the display must always show which one applies. Any value the display doesn't recognise must be treated as `unavailable`.

### Age and staleness without a clock

A display may not know the time. Measure everything on the hub's clock:

```text
age          = (generated_at − updated_at) + (time since the snapshot arrived, by a monotonic timer)
stale        = age > stale_after_seconds
reset in     = resets_at − (generated_at + time since the snapshot arrived)   (meter items)
```

Once a reset time has passed, show "reset due" instead of a countdown. The hub clears a meter item when its window has reset (the share becomes `null`, with a note), so the next snapshot corrects it.

### Size and bounds

A snapshot is never larger than **32 KiB** and a page never larger than **3072 bytes**; see Size above. The tests check that ten largest-possible pages, with four-byte characters everywhere, fit.

## Errors

Errors are JSON: `{"error": {"code": "...", "message": "..."}}`. The `message` is plain English for people. Act on the `code`:

| Codes | Display action |
|---|---|
| `token_revoked`, `unknown_device`, `enrolment_used`, `enrolment_expired` | Stop polling (retry hourly). Show that the display needs setting up again from the desktop manager. |
| `incompatible_version` | Show that the hub and display software don't match. Retry hourly. |
| `rate_limited`, `unavailable`, and any code you don't recognise | Keep the last good data, and retry with backoff. |
| `unauthorised`, `forbidden`, `bad_request`, `payload_too_large`, `not_found`, `conflict` | Treat as a fault. Keep the last good data, retry with the same backoff (up to 15 minutes), and log the code. |

An error with no JSON body, such as a proxy's page, counts as `unavailable` for a 5xx status and as a fault otherwise.

## What a display does

- **Keep the last good snapshot** in flash, with the time it arrived. Show it after a restart, a failed poll or an outage, marked with its age, and with stale pages marked as stale.
- **Accept a response** only if:
  - it is at most 32 KiB, which you check before parsing;
  - `protocol_version` is 1 and `message_type` is `dashboard.snapshot`;
  - its envelope and the common fields of every page follow the rules above.

  Otherwise keep the last good snapshot and say the hub sent something unreadable. A page whose `kind` or `data` you can't use is not a reason to refuse: show the card for that page.
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
| `firmware` | Optional: `{"trusted_keys": [1 to 3 public keys], "security_counter": n}`, from the package the app just installed over USB. A display checks each key is a valid P-256 point before storing anything, and refuses the bundle (`invalid_bundle`) otherwise. A display with no keys takes them; one that already has keys accepts the bundle only if it names the same set or leaves the member out. Accepting a bundle refills the display's attempt budget. See [Signed firmware updates](#signed-firmware-updates). |

The bundle is secret while it's valid. Keep the hub details and Wi-Fi settings, use the secret once, then discard it. **Never store provider credentials on a display.** None are ever sent.

### Over USB (serial)

The manager talks to the display over its USB serial port (CDC-ACM or USB-Serial-JTAG) at 115200 baud, using UTF-8 JSON, one object per line ending in `\n`, with each line at most 16 KiB. Lines that don't start with `{` are ignored by both sides, so boot logs on the same port are harmless.

| Step | Manager sends | Display answers |
|---|---|---|
| 1 | `{"type":"hello","protocol_version":1}` | `{"type":"hello","device_type":"llm-usage-display","firmware_version":"…","hardware_revision":"…","protocol_version":1,"provisioned":false}` |
| 2 | `{"type":"provision","bundle":{…}}` | `{"type":"provisioned"}`, or `{"type":"error","code":"invalid_bundle"\|"already_provisioned"\|"storage_failed"\|"firmware_keys_locked","message":"…"}` |

Rules:

- Each answer must arrive within 10 seconds.
- The manager repeats `hello` every 2 seconds until it gets an answer, in case the board restarted when the port opened. Answer every `hello`.
- The manager opens the port with DTR set and RTS clear, which doesn't reset an ESP32 through its USB serial bridge.
- A display that is already provisioned answers `already_provisioned`, and keeps its settings, until the owner factory-resets it on the device itself. Someone who plugs a cable in can't silently point it at another hub.
- A factory reset erases the Wi-Fi settings, trust anchors and token. It keeps the firmware keys and security floor.
- `firmware_keys_locked` means the display already trusts a different set of firmware keys, and only **Install firmware** (a USB install that erases it) can change them. An older manager shows it as an unrecognised error, with the message.
- After `provisioned`, the display joins the Wi-Fi network and enrols. The manager then watches for it to appear in the hub's list of displays.

`tools/display-simulator` stands in for the display at both stages: it enrols from a bundle file, and answers the serial exchange on a pseudo-terminal.

## Signed firmware updates

The full design, with the package format, what a display must check and the threat model, is [docs/firmware-updates.md](firmware-updates.md). These are its additions to v1. Every one is optional: older displays ignore the new members and an older hub never sends them.

**What a display reports.** The `firmware` member of its `device` object:

```json
"firmware": { "key_ids": ["<64 hex digits>"], "security_floor": 3, "slot_bytes": 3145728, "attempts_left": 5 }
```

`key_ids` (0 to 3, no repeats) are the SHA-256 of each trusted public key, `security_floor` the highest security counter it has confirmed, and `slot_bytes` the size of its spare app partition. `attempts_left` is optional, and worth sending: what is left of the display's attempt budget, 0 to 5. The hub uses them only to warn the owner before an offer the display would refuse, and to show what the display trusts and whether an update will have to wait. It never refuses an offer because `attempts_left` is 0.

**The attempt budget.** A display tries at most five downloads at once, and earns one back for each four hours of its own running time, measured with a monotonic timer that starts at zero at every start (never a date, and never the hub's time). An attempt's result, a restart and anything from the network leave the budget alone; only a provisioning bundle accepted over USB, or **Install firmware**, refills it. With no attempts left, a display doesn't request the download and doesn't report: the offer stays open. A display never acts on the offer it last acted on, and a refusal from the offer alone (`no_trusted_keys`, `too_large`) costs nothing. [docs/firmware-updates.md](firmware-updates.md), "Limits on trying", has the exact rules.

**The offer.** `firmware_update` in the snapshot (above). `size` is the length of the *update form*, at most 137 + 4096 + 8 MiB bytes. An offer lasts at most 24 hours, and a display has at most one.

**`GET /v1/device/firmware/{package_id}`.** Authenticated, with the same pinned TLS as everything else. `200` with `Content-Type: application/octet-stream`, an exact `Content-Length`, no compression and no chunked encoding; the body is the update form. A `Range` header is ignored: a download always starts from the beginning. Errors use the usual JSON body:

- `not_found` (404): this display has no open offer for that package;
- `rate_limited` (429): this display has started 6 downloads in the last hour;
- `unavailable` (503): the hub is already sending 2 downloads to other displays, or can't read the package. A new download from a display that is already downloading isn't refused: it takes over from the last one, whose connection the hub closes.

The request has its own 15-minute time limit, in place of the usual 10 seconds. A display gives up on a download that sends nothing for 30 seconds or takes more than 15 minutes, by its monotonic timer.

**`POST /v1/device/firmware/report`.** Authenticated; `204`. Body (`firmware-report.schema.json`):

```json
{ "offer_id": "<32 hex digits>", "state": "refused", "reason": "downgrade", "firmware_version": "1.1.0" }
```

| `state` | `reason` |
|---|---|
| `downloading` | none |
| `installed` | none |
| `refused` | `no_trusted_keys`, `too_large`, `bad_format`, `untrusted_key`, `bad_signature`, `wrong_device_type`, `wrong_hardware` or `downgrade` |
| `failed` | `download`, `image_hash` or `flash` |
| `rolled_back` | none |

Send `downloading` as the download starts and a final report when the offer is settled, repeating a final report until the hub answers. `firmware_version` is the version running when the report is sent, and a final report's version becomes the display's version on the hub. New firmware on its trial boot sends no report, and doesn't act on offers, until it is marked good; its check-in is a request that doesn't name the new version, such as fetching the snapshot. A final report for an offer already settled is answered `204` and changes nothing. `not_found` (404) means the display has no offer with that id. The hub records a state or reason it doesn't know as a failure.

**Provisioning and USB.** The bundle's optional `firmware` member, and the USB error code `firmware_keys_locked` (above). A bundle naming a key that isn't a valid P-256 point is answered `invalid_bundle`.

**Admin and publisher routes.** These are for the app and the signing tool, not displays; [docs/firmware-updates.md](firmware-updates.md) lists them. A publisher token (`dhp_…`) may only upload and list packages, may make room only by removing its own packages, and has 64 MiB of packages at most.

## Compatibility

- Displays **ignore fields they don't know**, and treat unknown `data_quality` values as `unavailable` and unknown error codes as transient. A hub may therefore add optional fields and new quality values within v1.
- New shapes and new custom kinds are additions too: a display that doesn't know a kind shows the card for that page and carries on with the others.
- A hub never removes or renames a field within v1, never changes a field's meaning, and never loosens a bound. A shape's `data` only ever gains optional fields. The published schema stays strict, describing exactly what a v1 hub sends.
- A breaking change needs `protocol_version: 2`. A hub serves every version from `MIN_SUPPORTED_PROTOCOL_VERSION` (currently 1) up to its own. A display that sends a version the hub doesn't serve gets `incompatible_version`.

## Finding the hub

The bundle lists one to four `hub.hosts` to try in order, such as `raspberrypi.local` then a fixed address. Resolving a `.local` name needs mDNS. ESP-IDF's lwIP resolves `name.local` through `gethostbyname()` when `CONFIG_LWIP_DNS_SUPPORT_MDNS_QUERIES` is enabled ([ESP-IDF lwIP guide](https://docs.espressif.com/projects/esp-idf/en/stable/api-guides/lwip.html)). Check on the board that your HTTP client resolves it that way too. Either way, try every host in the list, and recommend that owners add the Pi's fixed address (`advertise_hosts` in `hubd.toml`). The certificate check, not the address, proves it's the hub.

## Conformance

- `protocol/v1/examples/valid/*.json` must be accepted and drawn.
- `protocol/v1/examples/invalid/*.json` are things a hub never sends. A display treats each one of three ways:
  - **Refused**, keeping the last good snapshot: a wrong version or message type, a bad envelope or common page field, too many pages and an unreadable timestamp.
  - **Tolerated and shown:** `extra-field` and `shape-extra-field` (unknown fields are ignored), `unknown-quality` (read as unavailable), `missing-used-percent` (a missing `percent` reads as unknown) and `firmware-update-bad-offer-id` (a firmware offer the display can't use is ignored).
  - **A card for that page, with the others still shown:** an unknown kind, bad shape data, data on an unavailable page, a custom kind with bad data and a page over 3072 bytes.
- `crates/hub-protocol/tests/conformance.rs` shows the exact expectation for each file.
- **`protocol/v1/vectors/`** has 57 language-neutral cases: a hub response and how long it has been held, in; what the display must do and show, out. That covers every example above, each shape, cards, staleness, reset times, oversize and non-JSON bodies, and every error group. Port them to the firmware's unit tests ([vectors/README.md](../protocol/v1/vectors/README.md)).
- USB messages have `usb.schema.json` and examples in `examples/usb/`.
- The firmware report has `firmware-report.schema.json` and examples in `examples/device/`, beside a heartbeat and a bundle with their `firmware` members. The display's side of signed updates has vectors of its own under `protocol/v1/firmware/`: package checks, the attempt budget, the trial boot and provisioning bundles, as the reference display does them ([firmware/README.md](../protocol/v1/firmware/README.md)).
