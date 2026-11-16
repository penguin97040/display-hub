# Conformance vectors

Each file is one case: a hub response, how long the display has held it, and exactly what a v1 display must do and show. Firmware ports them to its own unit tests. They need no network, hub or Rust.

They come from the reference display (`tools/display-simulator`, `src/conform.rs`). `display-simulator conform check` proves these files still match it, and a test runs the same check in CI. After a deliberate change, regenerate them with `display-simulator conform write`.

## Format

```json
{
  "format": "display-hub.conformance",
  "version": 1,
  "name": "valid-all-shapes",
  "description": "…",
  "response": { "status": 200, "body": "<the exact response body, as text>" },
  "held_seconds": 4200,
  "expect": {
    "outcome": "show",
    "next_poll": "refresh",
    "refresh_after_seconds": 300,
    "error_code": null,
    "pages": [
      {
        "id": "claude-max", "title": "Claude Max", "show": "show",
        "cant_show_reason": null, "card_lines": [],
        "quality_label": "REPORTED", "note": null, "age_seconds": 4200, "stale": true,
        "content": {
          "shape": "meter",
          "items": [
            { "id": "five-hour", "label": "Five-hour allowance used", "percent": 42.0,
              "resets_in_seconds": 3000, "reset_due": false }
          ]
        }
      },
      {
        "id": "gauge", "title": "Gauge", "show": "cant_show",
        "cant_show_reason": "unknown_kind",
        "card_lines": ["This display can't show this page.", "It may need a firmware update."],
        "quality_label": "MANUAL", "note": null, "age_seconds": 4200, "stale": true,
        "content": null
      }
    ]
  }
}
```

| Field | Meaning |
|---|---|
| `response.status`, `response.body` | What `GET /v1/device/snapshot` returned. Feed the body to your parser byte for byte. |
| `held_seconds` | Time since the response arrived, by a monotonic timer. Ages and countdowns use the hub's `generated_at` plus this, never the display's wall clock. |
| `expect.outcome` | `show`: draw `pages`. `unreadable`: keep the last good snapshot and say the hub sent something unreadable. `reprovision`, `incompatible`, `retry` or `fault`: the error groups in `docs/protocol-v1.md`; keep the last good snapshot and show the matching message. |
| `expect.next_poll` | `refresh`: wait `refresh_after_seconds` (±10% jitter). `backoff`: 15 s, doubling to 15 min (±20%). `hourly`: try again in an hour. |
| `expect.error_code` | The hub's error code, if it sent one. Log it. |
| `pages[].show` | `show`: draw the page. `cant_show`: draw the "can't show this" card instead (the title and `card_lines`, nothing else about the page). One page being a card never affects the others. |
| `pages[].cant_show_reason` | Why, for a card: `unknown_kind` (a shape name or custom kind this display doesn't support), `bad_data` (the data breaks its shape's rules, is missing, or is on an unavailable page) or `too_large` (the page is over 3072 bytes). `null` when the page shows. |
| `pages[].card_lines` | The exact lines of the card, under the title. A second line, "It may need a firmware update.", appears for `unknown_kind` only. Empty when the page shows. |
| `pages[].quality_label` | What the screen must say about the figures. An unknown quality is `UNAVAILABLE`. |
| `pages[].age_seconds`, `stale` | `null` and `false` when the page has never had figures. |
| `pages[].content` | What to draw, by `shape`. `null` for an unavailable page (never draw its figures) and for a card. |
| `content.items[].percent` (meter) | `null` is unknown: draw "unknown", never 0%. |
| `content.items[].resets_in_seconds`, `reset_due` (meter) | A countdown while the reset is ahead; `reset_due` once it has passed. |
| `content.value`, `content.text` (big_number) | `value` `null` is unknown. `text` is the figure to draw: `decimals` places (0 if absent), or `-` for unknown. Never draw unknown as 0. |
| `content.text` (text) | The paragraph. |
| `content.items[]` (list) | `text` and optional `detail`, in order. May be empty. |

The reference display supports the four ready-made shapes and **no custom kinds**, so every custom kind is a card with reason `unknown_kind`. Firmware that does support a custom kind draws that page instead, and its tests for that kind are its own.

Compare `percent` exactly as decimals with one place; everything else is exact.

## What's covered

- Every valid example in `../examples/valid` (each shape, with null percent and null figure cases, and a custom kind shown as a card), and every invalid one in `../examples/invalid`.
- What a display refuses, tolerates or shows as a card:
  - **Refused** (keep the last good snapshot): a wrong version or message type, a bad envelope or page `id`, title, note, `updated_at` or `stale_after_seconds`, too many pages, and an unreadable timestamp.
  - **Tolerated and shown:** an extra field, an unknown quality (read as unavailable) and a missing `percent` (read as unknown).
  - **A card for that page only:** an unknown shape name, a custom kind with a bad version, bad shape data (too long, out of range, too many items, duplicate ids, a missing `data`), `data` on an unavailable page, a custom `data` that isn't an object or nests too deep, and a single page over 3072 bytes.
- Holding a snapshot until it goes stale, and past a reset time.
- A body over 32 KiB, and a captive portal's HTML.
- Every error group, an unknown error code, and errors with no structured body.
