# Setup over USB, end to end

The desktop app's real sender (`hub_client::usb`, the code behind **Add a display**) against the firmware's own handler (`dh_core`'s `dh_usb.h`), over a pseudo-terminal. No board is needed. Linux only: on macOS a pseudo-terminal can't be opened as a serial port.

The display end is `../usb_pty_display.c`, built with the host tests as `dh_usb_pty_display`. It keeps the setup, the firmware keys and the update record in a folder as the same blobs the firmware keeps in NVS, so stopping it and starting it again is a restart, and `--factory-reset` erases the setup and nothing else.

`src/main.rs` walks through what an owner does with a real board: `hello`, a bundle that breaks a rule, a good bundle, a second bundle (`already_provisioned`), a restart, a factory reset, a bundle naming other firmware keys (`firmware_keys_locked`), and the same keys again. Then it sends 66 bundles at the edge of each rule, the size limit included, and checks the firmware accepts exactly those the reference's own `Bundle::problems()` finds nothing wrong with.

## Running it

From the repository root, with Rust installed (`docs/developer-setup.md`) and Mbed TLS for the host tests. Keep both build folders outside the repository:

```sh
cmake -S firmware/host-tests -B /tmp/dh-host && cmake --build /tmp/dh-host --target dh_usb_pty_display
cargo run --manifest-path firmware/host-tests/usb-e2e/Cargo.toml --target-dir /tmp/dh-usb-e2e -- /tmp/dh-host/dh_usb_pty_display
```

It prints a line for each step and `all passed`. It is a workspace of its own, with its own `Cargo.lock` pinned to the same versions as the main one, so the main workspace's builds and checks don't include it, and it isn't part of `ctest`.

## By hand

`dh_usb_pty_display <folder>` prints a path such as `/dev/pts/7` and answers there until it is stopped, as `display-simulator usb` does for the reference display. Whatever can open the simulator's port can open this one.
