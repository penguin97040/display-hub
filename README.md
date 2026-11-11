# Display Hub

A small, private hub for desk displays. A Raspberry Pi runs the hub around the clock. Displays, starting with an ESP32 subscription-usage tracker, fetch their pages from it over the local Wi-Fi. A native desktop app, Windows first, configures it. Nothing goes through the cloud. Provider credentials never leave the Pi, and figures are always labelled with where they came from.

```text
 Windows PC                        Raspberry Pi 5                     ESP32 displays
 ┌──────────────┐  admin, HTTPS   ┌───────────────────┐  HTTPS poll  ┌────────────────┐
 │ hub-manager  │ ───────────────▶│ hubd (always on)  │◀──────────── │ llm-usage-     │
 │ (GPUI app)   │                 │ SQLite, snapshots │              │ display        │
 └──────────────┘                 └───────────────────┘              └────────────────┘
 optional PC helper (hubctl) ──────▲ forwards usage your tools already report
```

The desktop app doesn't need to stay open. When the PC is off, the displays keep showing the last figures the Pi has, with their age.

## Status

This is under active development. See [docs/architecture.md](docs/architecture.md) for the plan.

| Part | State |
|---|---|
| Protocol v1 types, schema and examples (`crates/hub-protocol`, `protocol/v1`) | Working, tested |
| Domain model, freshness and demo provider (`crates/hub-core`) | Working, tested |
| `hubd dev`: local demo hub over plain HTTP, loopback only | Working, tested |
| `display-simulator`: enrolment, polling, saved data, outages, text screen | Working, tested |
| `hubd serve`: HTTPS with a pinned identity, SQLite, admin pairing, display enrolment and revocation | Working, tested on Linux (including an mbedTLS client). Not yet on a Pi. |
| Raspberry Pi install (`deploy/raspberry-pi`): hardened systemd service, backups, upgrades | Tested in a systemd container and under ARM64 emulation. Not yet on a real Pi: [docs/deploy-raspberry-pi.md](docs/deploy-raspberry-pi.md). |
| Desktop manager (`apps/hub-manager`): pairing, pages, manual figures, displays, USB setup | Working, tested on Linux (including screenshots) and built for Windows. Not yet tried on a Windows desktop: [docs/windows-testing.md](docs/windows-testing.md). |

The ESP32 firmware lives in a separate repository, `llm-usage-display`. This repository defines the protocol it follows.

## Try it

You need [Rust](https://rustup.rs). `rust-toolchain.toml` pins the version, and `cargo` installs it the first time. In two terminals:

```sh
cargo run -p hubd -- dev                          # a demo hub on http://127.0.0.1:8080
cargo run -p display-simulator -- poll --max-wait 10
```

That is the quick demo, with no security. The real thing (pairing, enrolment and HTTPS) is in [docs/developer-setup.md](docs/developer-setup.md#run-a-real-hub-locally), and [docs/windows-testing.md](docs/windows-testing.md) walks through it with the desktop manager.

The simulator prints what a display would show for each page, with its data quality (`DEMO` here) and age. Stop the hub and the simulator switches to its saved data and marks it stale. Start it again and the simulator reconnects. `hubd dev --scenario mixed` shows unavailable and stale pages, and `--fault malformed|wrong-version|oversize|server-error|slow` reproduces faults.

[docs/developer-setup.md](docs/developer-setup.md) covers building, testing and the platform details.

## Licence

MIT, in [LICENSE](LICENSE). The Geist fonts in `assets/fonts` are under the SIL Open Font License (`assets/fonts/OFL-Geist.txt`). `vendor/gpui-pre-windows` is Apache-2.0, as described in [vendor/README.md](vendor/README.md).
