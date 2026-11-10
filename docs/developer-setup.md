# Developer setup

## Layout

| Path | What it is |
|---|---|
| `crates/hub-protocol` | Protocol v1: wire types, bounds and validation. No I/O. The contract with firmware. |
| `crates/hub-core` | Domain: subscriptions, data quality, freshness, providers and snapshot building. No I/O. |
| `crates/hub-client` | Client for the hub's HTTP API, used by the simulator, the manager and the PC helper. |
| `apps/hubd` | The hub service (library plus a command-line binary). |
| `apps/hub-manager` | The native desktop manager (GPUI). |
| `tools/display-simulator` | Emulates a display, for testing without hardware. |
| `protocol/v1` | JSON Schema and valid and invalid example snapshots. |
| `design/tokens.json` | The zvx.nz design tokens the manager follows (a copy; see the file's header). |
| `assets/fonts` | Geist and Geist Mono, bundled into the manager. |
| `vendor/gpui-pre-windows` | GPUI's Windows backend, changed so Linux can cross-build it ([vendor/README.md](../vendor/README.md)). |

## Toolchain

`rust-toolchain.toml` pins Rust 1.99.1 and lists the cross targets. Every dependency is pinned exactly in the root `Cargo.toml`. `Cargo.lock` is committed, and CI builds with `--locked`.

### Linux (Debian 13 or Ubuntu 24.04)

The hub, simulator and protocol crates need only a C toolchain. The desktop manager needs GPUI's libraries:

```sh
sudo apt-get install -y build-essential clang lld cmake pkg-config \
  libxkbcommon-x11-dev libxkbcommon-dev libwayland-dev libxcb1-dev libxcb-xkb-dev \
  libx11-xcb-dev libfontconfig-dev libfreetype-dev libasound2-dev libssl-dev \
  libvulkan1 mesa-vulkan-drivers
```

### Windows

Install Rust with `rustup` and the Visual Studio Build Tools (the "Desktop development with C++" workload). [windows-testing.md](windows-testing.md) has the exact steps once the manager is ready to try.

## Build and test

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check            # licences, advisories and sources (cargo install cargo-deny)
cargo audit                 # RustSec advisories (cargo install cargo-audit)
```

The tests need no network, hardware or credentials. Hubs run in-process on `127.0.0.1` with a fake clock.

## Run the demo hub and a simulated display

```sh
cargo run -p hubd -- dev [--scenario demo|mixed|empty] [--fault none|malformed|wrong-version|oversize|server-error|slow]
cargo run -p display-simulator -- once  [--hub http://127.0.0.1:8080/] [--cache sim-cache.json]
cargo run -p display-simulator -- poll  [--max-polls N] [--max-wait SECONDS]
cargo run -p display-simulator -- validate protocol/v1/examples/valid/*.json
```

`hubd dev` is plain HTTP with no authentication. It refuses any address other than loopback (`127.0.0.1` or `::1`), and must never be deployed. `display-simulator once` exits with `0` when online, `3` when showing saved data, and `4` when it has nothing to show.

## Cross-building the desktop manager for Windows from Linux

This build is for trying changes on a Windows machine. It compiles the shaders when the app first starts, using the system's `d3dcompiler_47.dll`.

```sh
sudo apt-get install -y mingw-w64
cargo build -p hub-manager --release --target x86_64-pc-windows-gnu
```

## Looking at the manager on Linux without a display

GPUI draws with Vulkan on Linux, and Mesa's software driver (lavapipe) works without a GPU:

```sh
Xvfb :201 -screen 0 1600x1000x24 &
DISPLAY=:201 openbox &
env -u WAYLAND_DISPLAY DISPLAY=:201 VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
  cargo run -p hub-manager -- --smoke-test     # prints "launched", then "rendered", and exits
```

This checks the Linux renderer only. It says nothing about how the app renders on Windows, which uses DirectX.

## Sandbox note

In the Docker `sbx` sandbox this repository was started in, `CARGO_TARGET_DIR` points at `target/sbx`. That keeps Linux build output apart from Windows builds made in `target/` on the host.
