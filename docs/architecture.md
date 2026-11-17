# Architecture

## The parts

| Part | Runs on | Job |
|---|---|---|
| `hubd` | Raspberry Pi 5, always on | Keeps the pages, what each shows, which pages each display shows, and the displays. Builds each display's snapshot. Serves the device and admin APIs over HTTPS on the local network. |
| ESP32 displays (`llm-usage-display` repo) | Desk, USB power only | Polls the hub over pinned HTTPS. Keeps the last good snapshot. Draws pages with LVGL. Swipes between them. |
| `hub-manager` | Windows PC (macOS and Linux later) | Pairs with the hub. Adds, removes and reorders pages. Enters manual figures. Adds and revokes displays. Shows diagnostics. Doesn't need to stay open. |
| `hub-send` | Any computer | The general "send data" tool and library: pairs a helper with the hub, keeps its token, and sends a page body to a feed ([helpers.md](helpers.md)). |
| `hub-templates` | Inside the manager (later) | Reads, checks and fills in page templates: a settings form and the pages it makes, as declarative JSON ([templates.md](templates.md)). The hub doesn't use it. |
| `usage-helper` | The PC | Optional helper that forwards usage figures your own tools already report (Claude Code, Codex) to the hub ([providers.md](providers.md)). It builds usage meter pages with the `usage` crate and sends them through `hub-send`'s library. |
| `display-simulator` | Any computer | Behaves like a display, for testing without hardware. |

## Data flow

1. The hub owns one ordered list of **pages**, up to 50. Each has a title, a **kind** (a ready-made shape, `meter`, `big_number`, `text` or `list`, or a custom `name/version` kind), a **source** (`demo`, `manual`, `helper` or `none`), optionally a **feed** (the named feed a helper fills it from, such as `garden-weather`) and how long its content stays fresh. The hub knows nothing more about what a page means. A usage subscription (Claude Max, ChatGPT Plus, OpenCode Go Plus) is just a `meter` page whose feed is the service's slug; the `usage` crate, which the manager and `usage-helper` use and the hub doesn't, knows which windows each service has.
2. What each page shows is stored as its `data`, in the shape its kind names, with its **data quality**: `demo`, `provider_reported`, `estimated`, `manual` or `unavailable`. These are never interchangeable, and every screen says which one it is showing.
3. Each display has a set of **ticked** pages. A new page is ticked on every display, and a new display gets every page; the manager can untick them.
4. When a display polls, the hub builds its **snapshot** from its first 10 ticked pages, in the shared order (`hub_core::build_snapshot`):
   - A demo page shows its stored sample content, labelled demo and dated now, so it never goes stale.
   - Stored content is checked, sanitised and bounded. A page that still breaks the page rules goes out as an unavailable page with a note instead, so it never blanks the others.
   - A meter item whose reset time has passed loses its old share: it becomes unknown, not zero.
5. The display draws exactly the pages in the snapshot, in that order. Adding, removing, reordering or ticking pages in the manager changes the next snapshot, with no firmware change.
6. Ages are measured on the hub's clock: `generated_at − updated_at`, plus however long the display has held the snapshot. A display without a working clock still shows correct ages and staleness.

## Storage

The hub keeps everything in one SQLite database, `hub.db` in its data folder (`/var/lib/display-hub` on a Pi), at schema version 5:

| Table | Holds |
|---|---|
| `pages` | Each page: id, title, kind, source, feed, position in the shared order, stale time, what was last stored for it (quality, `updated_at`, `data` as JSON text, note), and the template it was made from and its settings (JSON text), if any. |
| `display_pages` | Which pages each display shows: one row per display and ticked page. |
| `devices`, `credentials`, `one_time_codes` | Displays, the hashes of every token, and pairing and enrolment codes. |
| `helper_feeds` | The feeds each helper may send to. |
| `audit_log` | Security-relevant events, kept for 90 days. |

Schema 3 replaced version 2's `subscriptions` and `page_data` tables. The upgrade turned each subscription into a `meter` page with the same id, title, order and source, its service as the feed, and its stored figures, typed-in ones included, as meter data, so every display showed what it did before. Every display that wasn't revoked was ticked for every page. Schema 4 renamed the helper on the PC's old name, "collector", to "helper" wherever it was stored (page sources, credential roles and pairing codes), and turned each helper's services into feeds of the same names (`claude-max`, `chatgpt-plus`, `opencode-go-plus`), so the pairings and tokens the hub holds, and invites not yet used, keep working. A PC paired with the old `hubctl` tool needs `usage-helper` installed and `usage-helper login` run again with a fresh helper string, because `usage-helper` doesn't read what `hubctl` kept (see [providers.md](providers.md), "Upgrading from `hubctl`"). The hub no longer makes up demo figures, so each demo usage page with nothing stored was given the sample figures it showed, without reset times. Schema 5 added each page's `template` and `settings`, empty for every page that was already there; nothing else changed. Before upgrading an existing database, the hub saves a copy beside it (`hub-before-upgrade-schema-<old version>-<UTC time>.db`, owner-only), and the upgrade runs in one transaction ([deploy-raspberry-pi.md](deploy-raspberry-pi.md), "Upgrading").

## Admin API

Everything under `/v1/admin/` needs an admin token from `hubd pair-admin`, except `pair` itself. Bodies are JSON, at most 16 KiB; errors use the same body as the device API ([protocol-v1.md](protocol-v1.md), "Errors"). The types are in `crates/hub-core/src/api.rs`, and `crates/hub-client` calls them.

| Route | Does |
|---|---|
| `POST /v1/admin/pair` | Exchanges a pairing secret for an admin token. |
| `GET /v1/admin/status` | Version, clocks, identity, addresses, and how many pages, displays and admins. |
| `GET /v1/admin/pages` | Every page, in order, with what was last stored for it. |
| `POST /v1/admin/pages` | Adds a page (title, kind, source, and optionally feed, id, stale time, template and settings) at the end, ticked on every display. The hub holds at most 50. The id `order` is reserved for the route below (the schema 3 upgrade renamed any page that had it to `order-2`, or the next unused number). A feed name is a lower-case slug (`a-z` and `0-9` in words joined by single hyphens, up to 48 characters). A page with source `helper` needs a feed; it may be of any kind. A template id is written like a custom kind (`usage-limits/1`); the hub only records it. Settings are a JSON object of the answers to the template's form, at most 2048 bytes as JSON and three levels deep (`hub_core::model::settings_problem`), and not secret: the page's helpers can read them. Empty settings are the same as none. |
| `GET`, `PATCH`, `DELETE /v1/admin/pages/{id}` | One page; change its title, source, stale time or settings (a new source drops what was stored, and the helper rule above applies; new settings replace the old, and `{}` removes them); remove it. The kind and template can't change. Pages without a template or settings list without those fields. |
| `PUT /v1/admin/pages/order` | The new order, naming every page once. |
| `PUT /v1/admin/pages/{id}/content` | What a manual or demo page shows: `data` in its kind's shape, and a note, stored as `manual` or `demo` after the page's source. It is checked as the hub checks a page before sending it (the kind's rules and the 3072-byte page cap), and a refusal says which rule broke. A meter with no items (`{"items": []}`) is also accepted, and shows as "No figures yet". The page must keep its source until the write happens. |
| `GET /v1/admin/devices` | The displays. `DELETE /v1/admin/devices/{id}` revokes one and clears its ticks. |
| `GET`, `PUT /v1/admin/devices/{id}/pages` | The pages a display shows, as `{"ids": [...]}`. Order doesn't matter: displays share the page order. |
| `POST /v1/admin/enrolments` | A one-time secret to set up a display. |
| `GET /v1/admin/preview-snapshot` | The snapshot a display would get now. With `?display=<id>`, that display's; without, the first 10 of all pages. |
| `GET`, `POST /v1/admin/helpers`, `DELETE /v1/admin/helpers/{id}` | Helpers: list, invite (`{"label": ..., "feeds": [...]}`, at least one valid feed name), revoke. |

The manager still works in usage subscriptions. `hub-client` maps them onto meter pages with the `usage` crate, and leaves other pages where they are when it reorders. It checks typed-in figures against the service's windows (each item one of its windows, listed once, with a share from 0 to 100) and rounds shares to one decimal place before sending them (`usage::clean_figures`); the hub stores meter content as sent. A new demo usage page is given the service's sample figures the same way.

## Helper API

Makers writing their own helper: see [helpers.md](helpers.md).

A helper pairs with `POST /v1/helper/pair` (`{"secret": ...}` from its helper string), and gets `{"helper_id", "helper_token", "feeds"}`. The token (`dhc_…`) works only on the two routes below, and only for those feeds.

`PUT /v1/helper/feeds/{feed}` sends a whole page body:

```json
{ "kind": "meter",
  "data": { "items": [ { "id": "soil", "label": "Soil moisture", "percent": 41.5 } ] },
  "collected_at": "2026-11-10T19:59:00Z",
  "note": null }
```

- The hub writes it to every page whose feed is `{feed}`, whose source is `helper` and whose kind is `kind`, stored as `provider_reported`. It answers `{"updated": [...], "skipped": [...]}`: the pages it wrote to, and the pages of the feed that take content from a helper but are of another kind, which it left alone. A feed that no page uses yet is fine: both lists are empty.
- The content is checked with the same rules and 3072-byte cap as any page, against every page it would go to, and nothing is stored unless it passes for all of them. A meter with no items is accepted, as for typed-in content.
- `collected_at` (optional, by the helper's clock) is refused if it is more than five minutes ahead of the hub's clock, and is never stored later than the hub's clock. Without it, the content is dated now.
- A feed the helper wasn't invited for gets `forbidden`. Bodies are at most 16 KiB.

`GET /v1/helper/feeds/{feed}/settings` gives the template and settings of every page of the feed whose source is `helper`, in page order, as `{"pages": [{"page_id", "template", "settings"}]}` (`settings` is `{}` for a page with none). It reads nothing else about the pages. A feed the helper wasn't invited for gets `forbidden`, as above.

## Decisions

| Decision | Why |
|---|---|
| A typed JSON snapshot that the device draws itself. No HTML, images or layout language. | Small, safe to parse, testable, and the firmware stays in control of rendering. |
| Displays poll with outbound HTTPS. | A display needs no open port. Polls carry jitter and backoff. |
| Storage is a module inside `hubd`, not its own crate. | Only the hub touches SQLite. A separate crate would be boilerplate. |
| No provider code runs on the Pi. | No provider documents a usage API the Pi could call on its own (see `docs/providers.md`, milestone 5). Figures arrive from a helper on a PC, from manual entry, or not at all. |
| GPUI is pinned at `gpui-pre` 0.3.11, with a vendored Windows backend. | It matches md-viewer, a shipping app on the same stack. The vendored backend lets a Linux sandbox cross-build Windows executables. |
| ECDSA P-256 for the hub's TLS key, TLS 1.2 kept alongside 1.3, and a fixed certificate name. | This is what ESP-IDF's mbedTLS supports well (milestone 2). |
| Pi releases built with `cargo zigbuild` against glibc 2.36. | One binary runs on Raspberry Pi OS Bookworm and Trixie. CI checks the ceiling, and the binary ran under emulation on both. |
| The helper on the PC forwards only what Claude Code and Codex already report on the PC, and only to pages set to "Helper". Its token is scoped to feeds. | No provider credentials anywhere, no undocumented endpoints, and a stolen helper token can only write to its own feeds' helper pages. |
| The hub has one generic door for helpers, and no usage knowledge. Usage limits live in the `usage` crate, used by the manager and `usage-helper`. | Anyone can write a helper for a new kind of page without changing the hub, and the hub's attack surface doesn't grow with each service. |
| Templates are declarative JSON (fields, options, conditions and `{key}` placeholders, no scripting), read by the manager. The hub only records each page's template id and settings, checked for size, and lets the page's helpers read the settings. | A template from anyone can't run code, the hub stays generic, and one helper can serve pages set up in different ways. |
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
| M5 | Helpers: helper on the PC for Claude Code and Codex, manual entry for OpenCode Go | Done and tested against stand-ins; not yet against a live Claude Code or Codex |
| M6 | Firmware handoff: frozen contract, conformance vectors, provisioning spec | Done: [firmware-handoff.md](firmware-handoff.md). The firmware itself is built in `llm-usage-display`. |
