# Conformance vectors

Each file is one case: a hub response, how long the display has held it, and exactly what a v1 display must do and show. Firmware ports them to its own unit tests. They need no network, hub or Rust.

They come from the reference display (`tools/display-simulator`, `src/conform.rs`). `display-simulator conform check` proves these files still match it, and a test runs the same check in CI. After a deliberate change, regenerate them with `display-simulator conform write`.

## Format

```json
{
  "format": "display-hub.conformance",
  "version": 1,
  "name": "held-until-stale",
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
        "id": "claude-max", "title": "Claude Max", "quality_label": "DEMO", "note": null,
        "age_seconds": 4200, "stale": true,
        "metrics": [
          { "id": "five-hour", "label": "Five-hour allowance used", "used_percent": 42.0,
            "resets_in_seconds": 3000, "reset_due": false }
        ]
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
| `pages[].quality_label` | What the screen must say about the figures. An unknown quality is `UNAVAILABLE`. |
| `pages[].age_seconds`, `stale` | `null` and `false` when the page has never had figures. |
| `pages[].metrics` | Empty for an unavailable page: never draw its figures. |
| `metrics[].used_percent` | `null` is unknown: draw "unknown", never 0%. |
| `metrics[].resets_in_seconds`, `reset_due` | A countdown while the reset is ahead; `reset_due` once it has passed. |

Compare `used_percent` exactly as decimals with one place; everything else is exact.

## What's covered

- Every valid example in `../examples/valid`, and every invalid one in `../examples/invalid`. Three of those are tolerated and shown, as the protocol says: an extra field, an unknown quality and a missing `used_percent`.
- Holding a snapshot until it goes stale, and past a reset time.
- A body over 32 KiB, and a captive portal's HTML.
- Every error group, an unknown error code, and errors with no structured body.
