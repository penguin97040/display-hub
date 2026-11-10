# Vendored crates

## `gpui-pre-windows` 0.3.11

Zed's `gpui_windows` crate (the Windows backend of GPUI), as published on crates.io under the
name `gpui-pre-windows`, Apache-2.0 (`gpui-pre-windows/LICENSE-APACHE`). It is used through
`[patch.crates-io]` in the root `Cargo.toml`.

Upstream compiles its HLSL shaders with `fxc.exe` in `build.rs`, which only works on a Windows
build machine, so a Linux host can't build a Windows executable. This copy comes from the
md-viewer project, which made the change below, with its names adjusted for Display Hub:

1. A `precompiled-shaders` feature chooses how the app gets shader bytecode. With it on (a Windows
   host with the Windows SDK), `fxc.exe` compiles them at build time. With it off (the default,
   and the Linux cross-build), the app compiles them on first start with the system's
   `d3dcompiler_47.dll` and keeps the bytecode in `%LOCALAPPDATA%\display-hub-manager\shader-cache`.
2. `build.rs` fails the build if `DH_REQUIRE_PRECOMPILED_SHADERS` is set and the feature is off,
   so a release can't ship the slower path by accident.

Everything else is upstream's code, unchanged. To upgrade GPUI, bump `gpui-pre` and
`gpui-pre-platform` together, take the new published `gpui-pre-windows`, and re-apply the change.
