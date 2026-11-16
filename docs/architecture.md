# Architecture

## The parts

| Part | Runs on | Job |
|---|---|---|
| `hubd` | Raspberry Pi 5, always on | Keeps the pages, what each shows, which pages each display shows, and the displays. Builds each display's snapshot. Serves the device and admin APIs over HTTPS on the local network. |
| ESP32 displays (`llm-usage-display` repo) | Desk, USB power only | Polls the hub over pinned HTTPS. Keeps the last good snapshot. Draws pages with LVGL. Swipes between them. |
| `hub-manager` | Windows PC (macOS and Linux later) | Pairs with the hub. Adds, removes and reorders pages. Enters manual figures. Adds and revokes displays. Shows diagnostics. Doesn't need to stay open. |
| `hubctl` | The PC | Optional helper that forwards usage figures your own tools already report (Claude Code, Codex) to the hub ([providers.md](providers.md)). |
| `display-simulator` | Any computer | Behaves like a display, for testing without hardware. |

## Data flow

1. The hub owns one ordered list of **pages**, up to 50. Each has a title, a **kind** (a ready-made shape, `meter`, `big_number`, `text` or `list`, or a custom `name/version` kind), a **source** (`demo`, `manual`, `collector` or `none`), optionally a **feed** (the named feed that fills it, such as a usage service) and how long its content stays fresh. A usage subscription (Claude Max, ChatGPT Plus, OpenCode Go Plus) is a `meter` page whose feed is the service.
2. What each page shows is stored as its `data`, in the shape its kind names, with its **data quality**: `demo`, `provider_reported`, `estimated`, `manual` or `unavailable`. These are never interchangeable, and every screen says which one it is showing.
3. Each display has a set of **ticked** pages. A new page is ticked on every display, and a new display gets every page; the manager can untick them.
4. When a display polls, the hub builds its **snapshot** from its first 10 ticked pages, in the shared order (`hub_core::build_snapshot`):
   - Demo pages are generated fresh.
   - Stored content is checked, sanitised and bounded. A page that still breaks the page rules goes out as an unavailable page with a note instead, so it never blanks the others.
   - A meter item whose reset time has passed loses its old share: it becomes unknown, not zero.
5. The display draws exactly the pages in the snapshot, in that order. Adding, removing, reordering or ticking pages in the manager changes the next snapshot, with no firmware change.
6. Ages are measured on the hub's clock: `generated_at − updated_at`, plus however long the display has held the snapshot. A display without a working clock still shows correct ages and staleness.

## Storage

The hub keeps everything in one SQLite database, `hub.db` in its data folder (`/var/lib/display-hub` on a Pi), at schema version 3:

| Table | Holds |
|---|---|
| `pages` | Each page: id, title, kind, source, feed, position in the shared order, stale time, and what was last stored for it (quality, `updated_at`, `data` as JSON text, note). |
| `display_pages` | Which pages each display shows: one row per display and ticked page. |
| `devices`, `credentials`, `one_time_codes` | Displays, the hashes of every token, and pairing and enrolment codes. |
| `collector_scopes` | The feeds (today, usage services) each PC helper may report. |
| `audit_log` | Security-relevant events, kept for 90 days. |

Schema 3 replaced version 2's `subscriptions` and `page_data` tables. The upgrade turned each subscription into a `meter` page with the same id, title, order and source, its service as the feed, and its stored figures, typed-in ones included, as meter data, so every display showed what it did before. Every display that wasn't revoked was ticked for every page. Before upgrading an existing database, the hub saves a copy beside it (`hub-before-upgrade-schema-<old version>-<UTC time>.db`, owner-only), and the upgrade runs in one transaction ([deploy-raspberry-pi.md](deploy-raspberry-pi.md), "Upgrading").

## Admin API

Everything under `/v1/admin/` needs an admin token from `hubd pair-admin`, except `pair` itself. Bodies are JSON, at most 16 KiB; errors use the same body as the device API ([protocol-v1.md](protocol-v1.md), "Errors"). The types are in `crates/hub-core/src/api.rs`, and `crates/hub-client` calls them.

| Route | Does |
|---|---|
| `POST /v1/admin/pair` | Exchanges a pairing secret for an admin token. |
| `GET /v1/admin/status` | Version, clocks, identity, addresses, and how many pages, displays and admins. |
| `GET /v1/admin/pages` | Every page, in order, with what was last stored for it. |
| `POST /v1/admin/pages` | Adds a page (title, kind, source, and optionally feed, id and stale time) at the end, ticked on every display. The hub holds at most 50. The id `order` is reserved for the route below. For now, a page with source `collector` must be a `meter` with a feed. |
| `GET`, `PATCH`, `DELETE /v1/admin/pages/{id}` | One page; change its title, source or stale time (a new source drops what was stored, and the collector rule above applies); remove it. The kind can't change. |
| `PUT /v1/admin/pages/order` | The new order, naming every page once. |
| `PUT /v1/admin/pages/{id}/content` | What a manual page shows: `data` in its kind's shape, and a note. It is checked as the hub checks a page before sending it (the kind's rules and the 3072-byte page cap), and a refusal says which rule broke. A meter with no items (`{"items": []}`) is also accepted, and shows as "No figures yet". The page must still be manual when the write happens. |
| `GET /v1/admin/devices` | The displays. `DELETE /v1/admin/devices/{id}` revokes one and clears its ticks. |
| `GET`, `PUT /v1/admin/devices/{id}/pages` | The pages a display shows, as `{"ids": [...]}`. Order doesn't matter: displays share the page order. |
| `POST /v1/admin/enrolments` | A one-time secret to set up a display. |
| `GET /v1/admin/preview-snapshot` | The snapshot a display would get now. With `?display=<id>`, that display's; without, the first 10 of all pages. |
| `GET`, `POST /v1/admin/collectors`, `DELETE /v1/admin/collectors/{id}` | PC helpers: list, invite (scoped to services), revoke. |

The manager still works in usage subscriptions. `hub-client` maps them onto meter pages, and leaves other pages where they are when it reorders. It checks typed-in figures against the service's windows before sending them (`hub_core::api::clean_figures`, which the PC helper's reports go through on the hub too).

A PC helper reports with `POST /v1/collector/usage` and its own token. The hub writes the figures to every `meter` page whose feed is that service and whose source is `collector`, and only for services the helper's invite named.

## Decisions

| Decision | Why |
|---|---|
| A typed JSON snapshot that the device draws itself. No HTML, images or layout language. | Small, safe to parse, testable, and the firmware stays in control of rendering. |
| Displays poll with outbound HTTPS. | A display needs no open port. Polls carry jitter and backoff. |
| Storage is a module inside `hubd`, not its own crate. | Only the hub touches SQLite. A separate crate would be boilerplate. |
| No provider code runs on the Pi. | No provider documents a usage API the Pi could call on its own (see `docs/providers.md`, milestone 5). Figures arrive from a PC helper, from manual entry, or not at all. |
| GPUI is pinned at `gpui-pre` 0.3.11, with a vendored Windows backend. | It matches md-viewer, a shipping app on the same stack. The vendored backend lets a Linux sandbox cross-build Windows executables. |
| ECDSA P-256 for the hub's TLS key, TLS 1.2 kept alongside 1.3, and a fixed certificate name. | This is what ESP-IDF's mbedTLS supports well (milestone 2). |
| Pi releases built with `cargo zigbuild` against glibc 2.36. | One binary runs on Raspberry Pi OS Bookworm and Trixie. CI checks the ceiling, and the binary ran under emulation on both. |
| The PC helper forwards only what Claude Code and Codex already report on the PC, and only to pages set to "PC helper". Its token is scoped to services. | No provider credentials anywhere, no undocumented endpoints, and a stolen helper token can only write figures for its own services. |
| A hardened systemd unit: no capabilities, read-only system, local addresses only. | `systemd-analyze security` rates it 1.0. hubd's own checks stay in place; the unit is a second wall. |
| Backups are folders, and restores move the old hub aside. | Easy to copy with `scp`, and a mistaken restore loses nothing. |
| The manager's logic is a plain model (`apps/hub-manager/src/model.rs`) that returns operations; a worker runs them on a tokio runtime; GPUI only draws and forwards input. | The rules are unit-tested without a window, and one test drives the model against a real hub and a simulated display. |
| One operation at a time in the manager. | Changes reach the hub in the order they were made, and a failure is never hidden by a later success. |

## Milestones

| | Scope | State |
|---|---|---|
| M0 | Plan, toolchain, GPUI start-up check on Linux and a cross-built Windows executable | Done; the owner ran it on Windows (11/11/2026) |
| M1 | Protocol v1, demo hub on loopback, display simulator, offline tests | Done |
| M2 | SQLite, HTTPS with pinned identity, admin pairing, display enrolment, revocation | Done (Linux and mbedTLS; not yet on a Pi) |
| M3 | Desktop manager on GPUI, USB provisioning | Done on Linux and built for Windows; the Windows desktop run is the owner's to try |
| M4 | Raspberry Pi install: systemd, backups, ARM64 builds | Done in a systemd container and under emulation; the real Pi is the owner's to try |
| M5 | Collectors: PC helper for Claude Code and Codex, manual entry for OpenCode Go | Done and tested against stand-ins; not yet against a live Claude Code or Codex |
| M6 | Firmware handoff: frozen contract, conformance vectors, provisioning spec | Done: [firmware-handoff.md](firmware-handoff.md). The firmware itself is built in `llm-usage-display`. |
