# Third-party code in the firmware

The firmware is built with ESP-IDF and a few components from the [ESP Component Registry](https://components.espressif.com). Each component is pinned to one exact version in [`main/idf_component.yml`](main/idf_component.yml), and [`dependencies.lock`](dependencies.lock) records the hash of each one, which the build checks when it downloads them.

## The framework

| Name | Version | Licence | Source |
|---|---|---|---|
| ESP-IDF | v5.5.6 (commit `880a203ae603d08a94d06aa01e0d6761958e5e9e`) | Apache-2.0, with its own third-party parts (Mbed TLS 3.6 is Apache-2.0) | https://github.com/espressif/esp-idf |

## Components

| Name | Version | Licence | Source |
|---|---|---|---|
| `waveshare/esp32_s3_touch_amoled_1_8` | 2.0.3 | Apache-2.0 | https://github.com/waveshareteam/Waveshare-ESP32-components/tree/master/bsp/esp32_s3_touch_amoled_1_8 |
| `espressif/esp_lcd_co5300` | 2.2.0 | Apache-2.0 | https://github.com/espressif/esp-iot-solution/tree/master/components/display/lcd/esp_lcd_co5300 |
| `espressif/esp_lcd_touch` | 1.2.1 | Apache-2.0 | https://github.com/espressif/esp-bsp/tree/master/components/lcd_touch/esp_lcd_touch |
| `espressif/esp_lcd_touch_cst816s` | 1.1.2 | Apache-2.0 | https://github.com/espressif/esp-bsp/tree/master/components/lcd_touch/esp_lcd_touch_cst816s |
| `espressif/esp_lcd_touch_ft5x06` | 1.1.1 | Apache-2.0 | https://github.com/espressif/esp-bsp/tree/master/components/lcd_touch/esp_lcd_touch_ft5x06 |
| `lvgl/lvgl` | 9.6.0~1 | MIT (bundled libraries listed below) | https://github.com/lvgl/lvgl (commit `60b614c23c816ca5edc5f2840c9945eff7da0ad4`) |
| `espressif/esp_lvgl_port` | 2.9.0 | Apache-2.0 | https://github.com/espressif/esp-bsp/tree/master/components/esp_lvgl_port |
| `espressif/esp_io_expander` | 1.2.1 | Apache-2.0 | https://github.com/espressif/esp-bsp/tree/master/components/io_expander/esp_io_expander |
| `espressif/esp_io_expander_tca9554` | 2.0.3 | Apache-2.0 | https://github.com/espressif/esp-bsp/tree/master/components/io_expander/esp_io_expander_tca9554 |
| `espressif/esp_lcd_panel_io_additions` | 1.0.1~1 | Apache-2.0 | https://github.com/espressif/esp-iot-solution/tree/master/components/display/lcd/esp_lcd_panel_io_additions |
| `espressif/esp_codec_dev` | 1.5.11 | Apache-2.0 | https://github.com/espressif/esp-adf/tree/master/components/esp_codec_dev |
| `espressif/cmake_utilities` | 0.5.3 | Apache-2.0 | https://github.com/espressif/esp-iot-solution/tree/master/tools/cmake_utilities |

The board package needs `esp_lcd_panel_io_additions`, `esp_codec_dev` (the board has an audio codec) and the FT5x06 driver (V1 boards' touch chip), and two of the drivers need `cmake_utilities`. They are pinned here so that nothing changes without an edit to `idf_component.yml`.

## What was checked (18/11/2026)

- **Where they come from.** Each is published on components.espressif.com under its genuine namespace (`espressif`, `lvgl`, `waveshare`), and each registry entry names its source repository above. The board package's main source file is identical to the copy in Waveshare's GitHub repository.
- **The licence text itself, not the registry's label.** Since mid-2026 the registry labels most new versions "Custom" because they ship a licence file rather than naming one. Every Apache-2.0 component above ships the unchanged Apache License 2.0 text. LVGL ships the MIT licence.
- **What runs at build time.** The CMake files, Kconfig files and scripts of each component were read before the first build:
  - `cmake_utilities` adds build targets (merged image, compressed update, relinker) that run only when asked for by name, and options that are off by default. It also sets compiler colour output.
  - `esp_lvgl_port` defines `lvgl_port_create_c_image()`, which clones a repository and runs `pip install` when called. The firmware must not call it; convert images ahead of time instead.
  - LVGL's downloads (`FetchContent`) are only on its desktop build path, never on ESP-IDF's.
  - `esp_codec_dev` contains a prebuilt library for a ZL38063 voice chip, linked only when `CONFIG_CODEC_ZL38063_SUPPORT` is on. It is off by default and turned off explicitly in `sdkconfig.defaults`. (That library's folder carries an MIT licence from Microsemi.)
- **The downloads match.** The components the build downloaded were compared file by file with the archives that were read, and their hashes in `dependencies.lock` match the registry's.

## How the board package is used

The board code ([main/board/board.c](main/board/board.c)) calls the board package's `bsp_i2c_init`, `bsp_display_new`, `bsp_touch_new` and `bsp_display_brightness_set`, and the TCA9554 and LVGL port drivers directly. It does **not** call `bsp_display_start()`: in version 2.0.3 that registers the QSPI screen with `lvgl_port_add_disp_rgb()`, which makes `esp_lvgl_port` write RGB-panel callbacks into memory past the end of the CO5300 driver's data and finish each frame before the screen has received it. The screen is added with `lvgl_port_add_disp()` instead. Check this again when updating either component.

## Read for reference, not copied

- Waveshare's examples for this board, https://github.com/waveshareteam/ESP32-S3-Touch-AMOLED-1.8 (Apache-2.0), commit `01fd3200370a4417b257e8a4c185a04ef931342a`: the I/O expander's pins and start-up pulse, the V2 screen's 16-pixel column offset, and that their AXP2101 example changes no power output.
- XPowersLib's AXP2101 register list (MIT), as bundled in those examples: the addresses of the chip ID, status and output on/off registers, which the board test only reads. No XPowersLib code is used.

## The font

| Name | Version | Licence | Source |
|---|---|---|---|
| Geist Regular and SemiBold | 1.800 | SIL Open Font License 1.1, with no Reserved Font Name ([`assets/fonts/OFL-Geist.txt`](../assets/fonts/OFL-Geist.txt)) | https://github.com/vercel/geist-font |

The screens draw Geist, the font the desktop app uses. The two TrueType files are the repository's own copies in [`assets/fonts/`](../assets/fonts/) (`Geist-Regular.ttf`, SHA-256 `85a1c6b1…b8a117`; `Geist-SemiBold.ttf`, `b9f52322…f489b2`), built into the app unchanged by `main/CMakeLists.txt` (`EMBED_FILES`) and drawn by LVGL's Tiny TTF. The licence allows them to be bundled with software, unchanged or not, as long as the licence goes with them and the fonts aren't sold on their own; the licence file is in the repository beside them. They cover 728 characters: Latin with its accents (ā ē ī ō ū and their capitals included), the basic Cyrillic alphabet and common symbols, but not Greek, Chinese, Japanese or emoji. A character outside them is drawn as a hollow box.

## Libraries bundled inside LVGL

LVGL's `COPYRIGHTS.md` lists the libraries it carries. Only those turned on in the settings are built. All are permissive: BSD-2-Clause (barcode, LZ4), BSD-3-Clause (TLSF), MIT (QR code, ThorVG, RapidJSON with a BSD-3-Clause part, printf, FT800 driver), Zlib (LodePNG), MIT or Unlicense (TinyTTF), Apache-2.0 (AnimatedGIF), and TJpgDec's own permissive licence. The firmware turns on Tiny TTF (`CONFIG_LV_USE_TINY_TTF`), which is stb_truetype, MIT or Unlicense. FreeType support needs the separate FreeType component, which isn't used, and the demo font beside it is OFL-1.1.

## Updating a component

Change its version in `main/idf_component.yml`, then before building: read its licence file and changelog, and read anything it runs at build time (CMake files, Kconfig, `project_include.cmake`, scripts). Build, check the new `dependencies.lock`, and update the table above.
