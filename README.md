# Display Hub

Little screens around the house that show what you care about: the weather, a bus time, the garden's soil, a reminder, how much of a subscription you've used. Display Hub is the part in the middle that makes them easy.

- **A hub** runs on a small computer that stays on, such as a Raspberry Pi. It keeps the pages and hands them out.
- **Displays** are small ESP32 screens (an ESP32 is a cheap Wi-Fi chip). Each one fetches its pages from the hub over your home Wi-Fi, and swipes between them.
- **The desktop app** (`hub-manager`) sets everything up: it pairs with the hub, adds displays over a USB cable, chooses which pages each one shows, and installs firmware (the program inside a display) and its updates.
- **Helpers** are small programs that collect data and send it to the hub through one simple door (`hub-send`). Anyone can write one.

Everything stays on your own network. The hub never goes on the internet, nothing goes through a cloud service, and each display trusts only your hub. Every figure on a screen says where it came from: typed in, reported, estimated or demo.

```text
 Your computer                      Always-on computer                  Displays
 ┌──────────────┐  set-up, HTTPS   ┌──────────────────┐   HTTPS        ┌──────────────┐
 │ hub-manager  │ ────────────────▶│ hubd (the hub)   │◀────────────── │ ESP32 screen │
 └──────────────┘                  └──────────────────┘  every few     └──────────────┘
                                            ▲              minutes
 helpers, on any computer or on the hub ────┘ send data with hub-send
```

The app doesn't need to stay open. When your computer is off, the displays keep working, and if the hub goes quiet they show the last pages they had, with their age.

## What you need

- **An always-on computer for the hub.** A Raspberry Pi is recommended: the hub is tested on a Raspberry Pi 5 with Raspberry Pi OS Lite (64-bit).
- **A display.** The first supported one is the Waveshare ESP32-S3-Touch-AMOLED-1.8, with a USB-C cable to set it up. Its official firmware is still being written (see Status), so for now the display simulator stands in for it.
- **A computer for the app.** Windows and Linux work today. macOS support is being added.
- **Wi-Fi** on 2.4 GHz, which ESP32 displays need, with the hub and the displays on the same network.

## Getting going

<!-- RELEASE DOWNLOADS: where the ready-made downloads are. -->
> **Ready-made downloads** for Windows, Apple silicon Macs, Linux and the Raspberry Pi hub are coming as previews on the [releases page](https://github.com/penguin97040/display-hub/releases). [Installing Display Hub](docs/install.md) says how to check them, open them and set up the hub with one line. If no release is listed yet, or you'd rather, build Display Hub from this repository as below.
<!-- END RELEASE DOWNLOADS -->

You need [Rust](https://rustup.rs). `rust-toolchain.toml` pins the version, and `cargo` installs it the first time.

1. **Try it on one computer first, in two terminals.** This is a demo hub with sample pages and no security, on this computer only:

   ```sh
   cargo run -p hubd -- dev
   cargo run -p display-simulator -- poll --max-wait 10
   ```

   The simulator prints what a display would show for each page, with where its figures came from (`DEMO` here) and their age. Stop the hub and it switches to its saved pages, marked stale. Start the hub again and it reconnects.

2. **Put the hub on a Raspberry Pi.** [docs/deploy-raspberry-pi.md](docs/deploy-raspberry-pi.md) walks through it, from building the files to backups.

3. **Run the desktop app** and pair it with the hub. [docs/developer-setup.md](docs/developer-setup.md) has what Linux and Windows need to build it, and [docs/windows-testing.md](docs/windows-testing.md) is a step-by-step tour of the app with a simulated display.

## Pages

A page is what a display shows on one screen. Each page has a title and a **kind**:

- one of four ready-made shapes: a **meter** (up to six bars), a **big number**, a **short text** or a **list**; or
- a **custom** kind of your own, such as `nearby-flights/1`, drawn by firmware that knows it.

A page's content can be typed in from the app, sent by a helper, or sample content for trying things out. The app adds pages from **templates**: ready-made page sets with a short form, such as "Pi status" or "Usage limits". It also makes a page for a helper of your own: you give the kind and the feed, and it shows how to feed it.

Each display can show any of the hub's pages. They all share one page order today; a separate order for each display is being considered.

## For makers

You can build on Display Hub without changing it: a **helper** that sends your own data, a **template** that offers pages with a short form, or **firmware** for a new kind of display. Start with [BUILDING-A-DISPLAY.md](BUILDING-A-DISPLAY.md). There is a starter helper to copy in [starters/helper/](starters/helper/).

### Using an AI agent

Copy this, paste it into your agent, then say what display you want.

```text
I want to build something for Display Hub, an open-source system of small home displays:
https://github.com/penguin97040/display-hub

Read the repository first, in this order: README.md, then BUILDING-A-DISPLAY.md, then the
documents it points to for what I am making (docs/helpers.md for a helper, docs/templates.md
for a template, and docs/protocol-v1.md, docs/firmware-handoff.md and docs/firmware-updates.md
for display firmware). docs/protocol-v1.md is the contract between the hub and a display.
Follow "The rules not to break" in BUILDING-A-DISPLAY.md exactly.

Then build what fits what I describe:
- a helper: a small program that collects data and sends it to a page with hub-send
  (start from starters/helper/);
- a template: a JSON file that describes pages and a short settings form;
- firmware for a display that Display Hub doesn't support yet, signed with hub-firmware.

Before writing code, ask me what I want to see and on which display. Keep it simple and
explain things in plain words. Never ask me to paste a token, pairing string or password into
this chat: I will paste those into the terminal myself when a command asks for one.

You are done when the checks in "The finish line" of BUILDING-A-DISPLAY.md pass for what you
built. For a helper: it sends to a real hub (hubd serve with a scratch data folder) through
hub-send, and the page shows in display-simulator. For firmware: it passes the conformance
vectors in protocol/v1, behaves like display-simulator, and its package passes
hub-firmware inspect. If you changed anything in the Display Hub repository itself, cargo test
must pass too. Then tell me exactly what you ran and what happened.

What I want:
```

## Status

This is under active development, heading for a first preview release. [docs/architecture.md](docs/architecture.md) explains how it fits together.

| Part | Where it stands |
|---|---|
| The hub, `hubd` | Working: HTTPS with a pinned identity, pairing, displays, pages, helpers and firmware packages. Tested on a Raspberry Pi 5, in Raspberry Pi OS containers, and with an mbedTLS client (the TLS library ESP32 firmware uses). |
| The desktop app, `hub-manager` | Working on Windows and Linux: pairing, pages from templates and for your own helpers, typed-in content, which pages each display shows, adding displays over USB, and installing and updating firmware. Adding a display and updating it have been tried with the simulator; a real display waits for the official firmware. macOS is being added. |
| Display protocol v1 (`docs/protocol-v1.md`, `protocol/v1`) | Written, with JSON Schemas, examples and conformance vectors. `display-simulator` is the reference display. |
| Helpers: `hub-send` and helpers on the Pi | Working, tested against a real hub. Helpers on the Pi each run in a sandbox of their own: [docs/helpers.md](docs/helpers.md). |
| Templates | The format and the built-in templates work in the app. The app can't yet load a template from a file: [docs/templates.md](docs/templates.md). |
| Signed firmware updates | The hub, the app, the signing tool (`hub-firmware`) and the simulator are done: [docs/firmware-updates.md](docs/firmware-updates.md). An update has been delivered to the simulator through a real Pi. A real display waits for the official firmware. |
| Official firmware for the Waveshare ESP32-S3-Touch-AMOLED-1.8 | Not written yet. It will live in `firmware/` in this repository. |
| The usage-limits example (`apps/usage-helper`, `crates/usage`) | Forwards subscription usage figures that tools on your computer already report. Tested against stand-ins, not yet with the live tools: [docs/providers.md](docs/providers.md). |

[docs/developer-setup.md](docs/developer-setup.md) covers building, testing and the platform details, and [docs/security.md](docs/security.md) how it is kept safe.

## Licence

MIT, in [LICENSE](LICENSE). The Geist fonts in `assets/fonts` are under the SIL Open Font License (`assets/fonts/OFL-Geist.txt`). `vendor/gpui-pre-windows` is Apache-2.0, as described in [vendor/README.md](vendor/README.md).
