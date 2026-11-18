# dh_core

The display's protocol core: plain C11, no hardware, no ESP-IDF headers and no heap. It builds as an ESP-IDF component and as a plain library for the host tests in `firmware/host-tests`, which run every snapshot vector in `protocol/v1/vectors` and every example in `protocol/v1/examples`.

It follows the reference display, `tools/display-simulator`, and `crates/hub-protocol`, including some details the documents leave open (listed at the top of `src/dh_snapshot.c`).

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

Nothing keeps state between calls: every function works on what the caller passes in.

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

## Memory

A `dh_snapshot` is about 38 KB and a `dh_text_scratch` about 4.5 KB; keep them in static memory or PSRAM, not on a task's stack. Reading uses about 1 KB of stack of its own (measured with `-fstack-usage` for the ESP32-S3), plus whatever the C library's `strtod` and `snprintf` take. Read into a spare snapshot and swap it in on `DH_OUTCOME_SHOW`, so the last good one survives a refusal.

## Testing

See `firmware/host-tests` for the host tests and `firmware/fuzz` for the fuzz targets.
