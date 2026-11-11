# Developer setup

## Layout

| Path | What it is |
|---|---|
| `crates/hub-protocol` | Protocol v1: wire types, bounds and validation. No I/O. The contract with firmware. |
| `crates/hub-core` | Domain: subscriptions, data quality, freshness, providers and snapshot building. No I/O. |
| `crates/hub-client` | Client for the hub's HTTP API, used by the simulator, the manager and the PC helper. |
| `apps/hubd` | The hub service (library plus a command-line binary). |
| `apps/hub-manager` | The native desktop manager (GPUI). |
| `apps/hubctl` | The PC helper: forwards Claude Code and Codex usage figures ([providers.md](providers.md)). |
| `crates/os-secrets` | Tokens in the OS credential store, shared by the manager and `hubctl`. |
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

Install Rust with `rustup` and the Visual Studio Build Tools (the "Desktop development with C++" workload). [windows-testing.md](windows-testing.md) has the steps for trying the manager.

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

## Run a real hub locally

This is the same HTTPS hub that runs on the Pi, with its data in a scratch folder:

```sh
cargo run -p hubd -- serve --data-dir dev-data/hub --listen 127.0.0.1:8443
cargo run -p hubd -- pair-admin --data-dir dev-data/hub --listen 127.0.0.1:8443 --host 127.0.0.1
```

`pair-admin` prints a `dh1.…` pairing string. Paste it into the desktop manager:

```sh
cargo run -p hub-manager -- --config-dir dev-data/manager
```

`--config-dir` keeps the manager's settings out of your real configuration folder. Its credential still goes to the system's credential store (the Secret Service on Linux, so a keyring such as GNOME Keyring must be running).

To add a simulated display over "USB", start the simulator's serial end, then type the port it prints into **Displays**, **Add a display**, **Or type a port**:

```sh
cargo run -p display-simulator -- usb --state dev-data/display --once   # prints "Serial port: /dev/pts/N"
cargo run -p display-simulator -- poll --state dev-data/display
```

Or choose **Save as a file** and use `display-simulator enrol --bundle FILE --state DIR`. [windows-testing.md](windows-testing.md) is a full checklist.

Other commands:

- `hubd admin list|revoke <id>|revoke-all`
- `hubd fingerprint`

`dev-data/` is ignored by Git.

### mbedTLS interoperability

```sh
sudo apt-get install -y libmbedtls-dev     # Debian 13 has mbedTLS 3.6, as ESP-IDF 5.x uses
cargo build -p hubd && bash tools/mbedtls-interop/run.sh
```

Ubuntu 24.04 packages mbedTLS 2.28, which is too old. Build 3.6 from its release instead (CI does this), with `make -C <source> lib`, then run the script with `MBEDTLS_DIR=<source>`.

This builds a small C client configured like ESP-IDF's HTTP client and checks four things:

- TLS 1.2 and 1.3 work;
- authenticated routes answer;
- a different hub's key is refused;
- a wrong name is refused.

CI runs it too.

## The PC helper against a local hub

```sh
cargo run -p hubd -- pair-helper --data-dir dev-data/hub --listen 127.0.0.1:8443 --host 127.0.0.1 --service claude-max
cargo run -p hubctl -- --config-dir dev-data/helper login        # paste the string
echo '{"rate_limits":{"five_hour":{"used_percentage":42,"resets_at":1900000000}}}' \
  | cargo run -p hubctl -- --config-dir dev-data/helper claude-statusline
cargo run -p hubctl -- --config-dir dev-data/helper status
```

`pair-helper` makes the same invite as the manager's **Settings**, **PC helper**. The figures appear on Claude Max pages whose figures are set to **PC helper**.

## Building for the Raspberry Pi

```sh
cargo install --locked cargo-zigbuild    # and zig 0.15 on PATH
cargo zigbuild --release --locked -p hubd --target aarch64-unknown-linux-gnu.2.36
```

[deploy-raspberry-pi.md](deploy-raspberry-pi.md) covers the rest. To test the installer and service without a Pi, in a Debian container running systemd (Docker that allows a privileged container is needed):

```sh
cargo zigbuild --release --locked -p hubd -p display-simulator --target x86_64-unknown-linux-gnu.2.36
bash tools/pi-container-test/run.sh
```

## Cross-building the desktop manager for Windows from Linux

This build is for trying changes on a Windows machine. It compiles the shaders when the app first starts, using the system's `d3dcompiler_47.dll`.

```sh
sudo apt-get install -y mingw-w64
cargo build -p hub-manager --release --target x86_64-pc-windows-gnu
```

## Looking at the manager on Linux without a display

The screenshots in milestone 3 were taken this way, with `xdotool` driving the window. A Secret Service is needed for pairing: `dbus-run-session` with `gnome-keyring-daemon --unlock --components=secrets` works headless.

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
