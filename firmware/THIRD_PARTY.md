# Third-party code in the firmware

The firmware is built with ESP-IDF and a few components from the [ESP Component Registry](https://components.espressif.com). Each component is pinned to one exact version in [`main/idf_component.yml`](main/idf_component.yml), and [`dependencies.lock`](dependencies.lock) records the hash of each one, which the build checks when it downloads them.

The licence texts and copyright notices of everything below that ends up in the firmware are in part 2 of [`THIRD-PARTY-NOTICES.txt`](../THIRD-PARTY-NOTICES.txt), which every download carries. [Keeping the notices up to date](#keeping-the-notices-up-to-date) says how that part is made.

## The framework

| Name | Version | Licence | Source |
|---|---|---|---|
| ESP-IDF | v5.5.6 (commit `880a203ae603d08a94d06aa01e0d6761958e5e9e`) | Apache-2.0, with its own third-party parts (below) | https://github.com/espressif/esp-idf |

ESP-IDF brings in parts from other projects. These are the ones with code in this firmware, from the linker's map files of a release build (ESP-IDF's own list of them is `docs/en/COPYRIGHT.rst` in its source):

| Part | Licence |
|---|---|
| FreeRTOS kernel, with Cadence's Xtensa port | MIT |
| Xtensa HAL library and headers (Cadence) | MIT |
| lwIP, the TCP/IP stack | BSD-3-Clause |
| Mbed TLS 3.6 | Apache-2.0 (offered as Apache-2.0 or GPL-2.0-or-later) |
| wpa_supplicant, Wi-Fi security | BSD-3-Clause |
| FreeBSD net80211, inside Espressif's Wi-Fi library | BSD-2-Clause |
| TLSF memory allocator (ESP-IDF's heap) | BSD-3-Clause |
| http_parser (ESP-IDF's HTTP client) | MIT |
| UBSAN runtime (`esp_system/ubsan.c`) | BSD-2-Clause |
| newlib C library, from Espressif's toolchain | BSD and similar permissive licences (`COPYING.NEWLIB`) |
| GCC's libgcc and libstdc++ | GPL-3.0 with the GCC Runtime Library Exception (no notice needed) |

Espressif's Wi-Fi and radio libraries (`esp32-wifi-lib`, `esp-phy-lib`) come as object code, under Apache-2.0 like the rest of ESP-IDF. Parts that ESP-IDF has but that aren't in this firmware, such as cJSON, FatFS, SPIFFS, the SD card drivers and Bluetooth, aren't listed.

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

Three of LVGL's extras end up in the firmware:

- **Tiny TTF** (stb_truetype, MIT or Unlicense), which draws Geist.
- **printf** (MIT), LVGL's built-in text formatting (`CONFIG_LV_USE_BUILTIN_SPRINTF`).
- **Montserrat 20**, LVGL's default font (`CONFIG_LV_FONT_DEFAULT_MONTSERRAT_20`). It is Montserrat (OFL-1.1) with a few Font Awesome Free 5 symbols (OFL-1.1 as a font). The screens draw Geist; Montserrat is in the image as LVGL's default.

The board package's other dependencies (`esp_codec_dev`, `esp_lcd_panel_io_additions`, `cmake_utilities`) are part of the build but have no code in the firmware.

## Updating a component

Change its version in `main/idf_component.yml`, then before building: read its licence file and changelog, and read anything it runs at build time (CMake files, Kconfig, `project_include.cmake`, scripts). Build, check the new `dependencies.lock`, and update the table above. Then check the notices, as below.

## Keeping the notices up to date

Part 2 of `THIRD-PARTY-NOTICES.txt` comes from [`tools/notices/firmware.txt`](../tools/notices/firmware.txt), written from a release build: its licence texts are copied from the files in ESP-IDF v5.5.6 and the components, and its list of parts from the build's map files. [`tools/notices/firmware-parts.txt`](../tools/notices/firmware-parts.txt) is that list as [`tools/notices/firmware-parts.py`](../tools/notices/firmware-parts.py) prints it: each library with code in the firmware or its bootloader, and LVGL's bundled libraries and built-in fonts that are in it.

CI and the release build the firmware and compare the list with the committed one. If they differ (after a new component, a new ESP-IDF, or a setting that brings in more code), find what is new in the map files, add its licence text and copyright notices to `tools/notices/firmware.txt` (and to the tables here), save the new list with `python3 -I tools/notices/firmware-parts.py <build folder> > tools/notices/firmware-parts.txt`, and run `bash tools/notices/generate.sh`.
