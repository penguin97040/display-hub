# Architecture

## The parts

| Part | Runs on | Job |
|---|---|---|
| `hubd` | Raspberry Pi 5, always on | Keeps the subscription list, stored figures and displays. Builds each display's snapshot. Serves the device and admin APIs over HTTPS on the local network. |
| ESP32 displays (`llm-usage-display` repo) | Desk, USB power only | Polls the hub over pinned HTTPS. Keeps the last good snapshot. Draws pages with LVGL. Swipes between them. |
| `hub-manager` | Windows PC (macOS and Linux later) | Pairs with the hub. Adds, removes and reorders pages. Enters manual figures. Adds and revokes displays. Shows diagnostics. Doesn't need to stay open. |
| `hubctl` (milestone 5) | The PC | Optional helper that forwards usage figures your own tools already report (Claude Code, Codex) to the hub. |
| `display-simulator` | Any computer | Behaves like a display, for testing without hardware. |

## Data flow

1. The hub owns an ordered list of **subscriptions**. Each is one page on the display: a service (Claude Max, ChatGPT Plus, OpenCode Go Plus), a title and a **source** (`demo`, `manual`, `collector` or `none`).
2. Figures for each subscription are stored with their **data quality**: `demo`, `provider_reported`, `estimated`, `manual` or `unavailable`. These are never interchangeable, and every screen says which one it is showing.
3. When a display polls, the hub builds a **snapshot** from the list in order (`hub_core::build_snapshot`):
   - Demo pages are generated fresh.
   - Stored figures are checked, sanitised and bounded.
   - A window whose reset time has passed loses its old figure: the share becomes unknown, not zero.
4. The display draws exactly the pages in the snapshot, in that order. Adding, removing or reordering subscriptions in the manager changes the next snapshot, with no firmware change.
5. Ages are measured on the hub's clock: `generated_at − updated_at`, plus however long the display has held the snapshot. A display without a working clock still shows correct ages and staleness.

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
| M5 | Collectors: PC helper for Claude Code and Codex, manual entry for OpenCode Go | Next |
| M6 | Firmware handoff: frozen contract, conformance vectors, provisioning spec | |
