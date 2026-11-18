# Display Hub firmware

The official firmware for a Display Hub display, written in C with ESP-IDF. It is also the starter project for anyone making firmware for their own display.

**Status: early.** For now it is a board test: it starts the screen, touch and power reading and shows what it finds (see [Board test](#board-test)). Setup over USB, Wi-Fi, the hub protocol and signed updates follow. [docs/firmware-handoff.md](../docs/firmware-handoff.md) is the brief it is built to.

## The board

Waveshare **ESP32-S3-Touch-AMOLED-1.8**, **V2 only** (CO5300 screen, CST820 touch; the revision is marked on the back). ESP32-S3 with 16 MB of flash and 8 MB of PSRAM, 368 × 448 screen.

| | |
|---|---|
| `device_type` | `display-hub-waveshare-amoled-1.8` |
| `hardware_revision` | `waveshare-amoled-1.8-v2` |
| Version | 0.1.0, set by `PROJECT_VER` in [CMakeLists.txt](CMakeLists.txt) |

## Board test

Install it with the desktop app's **Install firmware**. It has three screens; swipe left or right to move between them (the dots at the bottom show which one you're on):

1. **About the board:** the firmware version, the app slot it is running from and its address (`ota_0` at `0x20000` after a first install), what the boot record says (`blank` after a first install), the board revision (V2 if the CST820 touch chip answers; a V1 board shows "not supported yet"), whether USB power is good, and free PSRAM and heap.
2. **Touch test:** a dot follows your finger, and the last swipe is shown in words.
3. **eFuse keys:** for each key block KEY0 to KEY5, its purpose, whether it is read-protected or write-protected, and whether it is empty; then whether download mode, secure download mode and USB-Serial-JTAG are turned off by eFuse, and whether Secure Boot and flash encryption are on. **It only reads**: nothing in this firmware writes an eFuse.

The same facts are printed to the console (USB-Serial-JTAG) every 10 seconds, and each swipe as it happens.

The board code is in [main/board/](main/board/). It uses the board package's screen, touch and brightness functions, but not its `bsp_display_start()`, which hands this QSPI screen to LVGL as if it were an RGB panel. It pulses the I/O expander's screen power and resets as Waveshare's examples do, and only reads the AXP2101 power chip.

## Building

You need **ESP-IDF v5.5.6** exactly, installed with Espressif's [instructions](https://docs.espressif.com/projects/esp-idf/en/v5.5.6/esp32s3/get-started/index.html) (for ESP32-S3). Other versions are refused by `main/idf_component.yml`. Then, from the repository's root folder:

```sh
. "$IDF_PATH/export.sh"
idf.py -C firmware -B firmware/build build
```

The first build downloads the components listed in [THIRD_PARTY.md](THIRD_PARTY.md) into `firmware/managed_components/`. The outputs land in the build folder: `display-hub.bin` (the app), `bootloader/bootloader.bin`, `partition_table/partition-table.bin`, `ota_data_initial.bin`, and `flasher_args.json`, which says where each goes.

Or build with Espressif's Docker image, as the GitHub checks do: `espressif/idf:v5.5.6`.

The settings are in [sdkconfig.defaults](sdkconfig.defaults), with a note on each. The build makes its full `sdkconfig` in the build folder, so change settings in `sdkconfig.defaults`, not with `menuconfig`. Builds are reproducible: the same source and ESP-IDF give the same image.

## Packing a development build

[tools/package-dev.sh](tools/package-dev.sh) signs a build with a key from `hub-firmware keygen` and makes a package the desktop app can install over USB:

```sh
firmware/tools/package-dev.sh firmware/build my-dev.dhkey display-hub-dev.dhfw
```

It reads the offsets from the build's `flasher_args.json` and the version from the built app, and asks for the key's passphrase (or set `DH_PASSPHRASE_FILE`). The key becomes the package's whole key set, so only use it for boards you're testing on. Never commit a key file.

## The flash layout

From [partitions.csv](partitions.csv). A first install sets it for good: an over-the-air update writes only the spare app slot.

| Part | Offset | Size |
|---|---|---|
| Bootloader | 0x0 | |
| Partition table | 0x8000 | |
| NVS (settings) | 0x9000 | 64 KiB |
| OTA data (which slot to boot) | 0x19000 | 8 KiB |
| PHY calibration | 0x1b000 | 4 KiB |
| App slot `ota_0` (a first install writes here) | 0x20000 | 6 MiB |
| App slot `ota_1` | 0x620000 | 6 MiB |
| Free | 0xc20000 | about 3.9 MiB |

There is no factory slot. Secure Boot and flash encryption are off; NVS encryption is off until it has been checked on a real board (see the handoff, "Storage and NVS").
