# Firmware update vectors

Each file is one case from `docs/firmware-updates.md`, "What the simulator must test": the display's state before, what arrives, and exactly what a display must do. Firmware ports them to its own unit tests. They need no network, hub or Rust.

They come from the reference display (`tools/display-simulator`, `src/firmware/`). `display-simulator conform check` runs every file here (and the snapshot vectors in `../vectors`) through it, and a test does the same in CI.

## Signatures are random, so these are checked, not regenerated

`ring` signs with a random nonce, so the same package signed twice gives two different, equally valid files. The vectors were made once, with throwaway keys made for the purpose. **Only the public keys are here**; the private halves existed only while the vectors were written, and were never saved.

- `display-simulator conform check` proves each file's `expect` is what the reference display does with its inputs.
- A fresh generation must agree with every file in everything except the key values and the package bytes (`stable` in `src/firmware/vectors.rs`). The test fails if a case is missing or out of date.
- `display-simulator conform write` writes only the vectors that are missing or out of date, and keeps the rest byte for byte. Running it with nothing to change changes nothing.

## Common fields

```json
{
  "format": "display-hub.firmware-conformance",
  "version": 1,
  "name": "package-07-untrusted-key",
  "cases": [7],
  "description": "Signed by a key the display doesn't hold, otherwise perfect.",
  "keys": { "maker": "04…", "recovery": "04…", "new": "04…", "stranger": "04…", "retired": "04…" },
  "kind": "package",
  …
}
```

| Field | Meaning |
|---|---|
| `cases` | The numbered cases in the document's list that this file covers. |
| `keys` | Public keys by name, as 130 lower-case hex digits (`04`, X, Y). Everywhere else a vector names a key, it uses these names. |
| `kind` | `package`, `offer`, `trial` or `provision`, below. |

## `package`: one offer, through every check

```json
"display": {
  "device_type": "llm-usage-display", "hardware_revision": "simulated", "firmware_version": "1.1.0",
  "trusted_keys": ["maker", "recovery"], "security_floor": 3, "slot_bytes": 65536
},
"offer": { "offer_id": "<32 hex digits>", "size": 892 },
"response": { "content_length": 892, "body_hex": "4448465701…", "after_body": "ends" },
"expect": {
  "state": "refused", "reason": "untrusted_key", "reports": ["refused"],
  "requests_download": true, "body_bytes_read": 137, "spare_slot": "untouched",
  "after_marked_good": null
}
```

| Field | Meaning |
|---|---|
| `display` | What the display knows about itself. `slot_bytes` is the size of its spare app partition. |
| `offer.size` | The snapshot's `firmware_update.size`. |
| `response.content_length` | The download's `Content-Length`. `null`: there was none (a chunked answer). |
| `response.body_hex` | The body. It may be shorter than `content_length` (it ends early) or longer (bytes the display must not read). |
| `response.after_body` | `ends`: the connection closes after the body. `stalls`: it stays open and sends nothing; the display gives up after 30 seconds. |
| `expect.state`, `reason` | The final report. `installed` means written, verified and set to boot, then marked good once the trial boot reaches the hub. |
| `expect.reports` | Every report, in order: `downloading` once the manifest has passed every check, then the final one. Only the final one is repeated until the hub answers. |
| `expect.requests_download` | `false` when the display refuses from the offer alone (no keys, or a `size` over 137 + 4096 + `slot_bytes`). |
| `expect.body_bytes_read` | How much of the body the display takes. `0` when it refuses from the length alone. Never more than the update form. |
| `expect.spare_slot` | `untouched` (never opened, so nothing written), `abandoned` (opened, then given up with `esp_ota_abort`) or `set_to_boot`. |
| `expect.after_marked_good` | For `installed`: the version, floor and trusted keys once the new firmware is marked good (the floor raised to the release's counter, the keys replaced by its `key_set`). |

Feed `body_hex` to your download code in pieces of any size, and check every boundary: the vectors' reference feeds 97 bytes at a time.

## `offer`: the attempt budget, and which offers to act on

```json
"before": { "attempts_left": 0, "credit_hours": 0, "last_offer_id": "<32 hex digits>" },
"events": [
  { "event": "running", "minutes": 239 },
  { "event": "offer", "offer_id": "<32 hex digits>", "outcome": "installed" },
  { "event": "running", "minutes": 1 },
  { "event": "offer", "offer_id": "<32 hex digits>", "outcome": "installed" }
],
"expect": {
  "actions": ["wait", "attempt"], "saves": 5,
  "after": { "attempts_left": 0, "credit_hours": 0, "last_offer_id": "<32 hex digits>" }
}
```

`before` is what the update record holds: attempts left (0 to 5), credit hours counted towards the next (0 to 3; always 0 with a full budget) and the offer last acted on. The display has just started, so its running time is zero. Then the `events` happen in order:

| `event` | Meaning |
|---|---|
| `offer` | An offer in a snapshot. `outcome` is what acting on it would come to: `refused_before_request` (`no_trusted_keys` or `too_large`, decided from the offer alone), or an attempt's final result, `installed`, `refused`, `failed` or `rolled_back`. The result never changes the budget. |
| `running` | The display keeps running for `minutes`, by its monotonic timer. Each full hour counted while the budget is below 5 is saved as a credit hour; the fourth turns into an attempt. |
| `restart` | A restart or power cut. The timer starts again at zero: time not yet saved as a credit hour is lost, and so is the memory of a refusal before any request. |
| `usb_bundle` | A provisioning bundle accepted over USB: the budget returns to 5. |
| `clock_moves` | The display's idea of the date (`"clock": "date"`) or the hub's time in its snapshots (`"hub"`) jumps by `seconds`, either way. Nothing changes. |

| `expect` | Meaning |
|---|---|
| `actions` | One for each `offer` event: `attempt` (one attempt spent, saved with the offer id in a single write, then the download requested), `refuse_without_request` (nothing spent or written), `ignore` (the offer last acted on, or one refused before any request since the last start) or `wait` (no attempts left: nothing requested, nothing reported, and the offer stays open). |
| `saves` | How many times the display saves its budget: once for each attempt, each credit hour and each refill that changes it. Nothing else writes it, so a full budget means `0`. A display that saves a credit hour at each full hour gets exactly this count. |
| `after` | The record afterwards. |

## `trial`: the trial boot

```json
"before": {
  "firmware_version": "1.1.0", "trusted_keys": ["maker", "recovery"], "security_floor": 3,
  "update": { "firmware_version": "1.2.0", "security_counter": 4, "key_set": null }
},
"trial": {
  "starts": true,
  "check_ins": [{ "at_seconds": 15, "answer": "no_answer" }, { "at_seconds": 45, "answer": "success" }],
  "restart_at_seconds": null
},
"expect": {
  "outcome": "marked_good", "at_seconds": 45, "report": "installed",
  "firmware_version": "1.2.0", "security_floor": 4, "trusted_keys": ["maker", "recovery"]
}
```

| Field | Meaning |
|---|---|
| `trial.starts` | `false`: the new firmware crashes before it can check in, so the board resets and rolls back. |
| `trial.check_ins` | Authenticated check-ins over pinned TLS, by seconds of the display's running time since the trial boot began (its monotonic timer, never a date or the hub's clock). A check-in doesn't name the new version: the trial firmware sends no heartbeat or report until it is marked good. `answer` is `success`, `no_answer`, or the hub's error code. Only `success`, `token_revoked` and `unknown_device` count as reaching the hub. |
| `trial.restart_at_seconds` | The board restarts or loses power then, before it is marked good: the bootloader rolls back. |
| `expect.outcome`, `at_seconds` | `marked_good`, or `rolled_back` (at 300 seconds if nothing else ends the trial first; an answer at 300 seconds or later is too late). |
| `expect.report` | `installed` (sent by the new firmware) or `rolled_back` (sent by the old firmware when it finds the update record unfinished). |
| `expect.firmware_version`, `security_floor`, `trusted_keys` | The display afterwards. A rollback drops the pending `key_set` and leaves the floor alone. |

## `provision`: a bundle over USB, or a factory reset

```json
"before": { "trusted_keys": ["maker", "recovery"], "security_floor": 3 },
"action": { "action": "bundle", "bundle_firmware": { "trusted_keys": ["stranger"], "security_counter": 0 } },
"expect": { "answer": "firmware_keys_locked", "trusted_keys": ["maker", "recovery"], "security_floor": 3 }
```

`action` is `bundle` (a provisioning bundle arrives over USB at a display that has been factory-reset or never set up; `bundle_firmware` is its `firmware` member, or `null`) or `factory_reset`. `answer` is the USB answer, `provisioned`, `firmware_keys_locked` or `invalid_bundle`, and `null` for a factory reset. A display checks first that every key in the bundle is a valid P-256 point, and refuses the bundle (`invalid_bundle`) storing nothing if one isn't; such a key can't be in `keys`, so the bundle writes it out in full as 130 hex digits instead of a name. A display with keys accepts a bundle naming the same set in any order, or none, and changes nothing; a display with none takes the bundle's keys and its counter as the floor. Accepting any bundle also refills the attempt budget (see the `offer` vectors).

## What's covered, and what isn't here

| Cases | Where |
|---|---|
| 1 to 23: package checks | `package-01-…` to `package-23-…` |
| 24 to 26: the offer last acted on, and the attempt budget | `offer-24-…` to `offer-26-…` |
| 29 to 32: the trial boot | `trial-29-…` to `trial-32-…` |
| 34 to 37: keys, including a `key_set` that drops a trusted key without raising the counter | `package-34-…`, `package-35-…`, `trial-34-…`, `trial-36-…`, `package-37-…` |
| 38 to 40: factory reset and bundles, including a key that isn't a valid point | `provision-38-…` to `provision-40-…` |

Case 33 (a power cut during the download) is about what survives a restart, so it is tested end to end in `tools/display-simulator/tests/firmware.rs` rather than here. Cases 27, 28 and 41 to 46 are the hub's part, tested against the real hub in `apps/hubd/tests/firmware.rs` and the simulator's tests. A display only needs to know that a hub may send it anything; the `package` vectors are what it does about that.
