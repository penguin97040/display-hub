# Display protocol, version 1

**Status: draft.** The snapshot format below is implemented and tested. Enrolment, heartbeat and transport security are added in milestone 2, and the whole contract is frozen when that milestone ends. After that, v1 changes only by adding things (see Compatibility).

This document is for anyone writing display firmware. The machine-readable parts are `protocol/v1/snapshot.schema.json` and the examples in `protocol/v1/examples/`. The Rust reference implementation is `crates/hub-protocol`.

## Transport

- The display makes outbound HTTPS requests to the hub on the local network. It never listens on a port.
- JSON bodies are UTF-8. Every request carries `X-Display-Protocol: 1`.
- **Development only:** `hubd dev` serves the same snapshot over plain HTTP on `127.0.0.1`, with no authentication. Firmware never talks to it.

## `GET /v1/health`

No authentication. It returns `{"status": "ok", "protocol_versions": [1]}` and nothing else, never account or credential details.

## `GET /v1/device/snapshot`

Returns the display's dashboard. From milestone 2 it requires the display's credential, and the hub takes the display's identity from that credential only, never from anything the request names.

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
