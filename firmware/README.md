# Display Hub firmware

The official firmware for a Display Hub display, written in C with ESP-IDF. It is also the starter project for anyone making firmware for their own display.

**Status: early.** It takes its setup over USB from the desktop app, keeps it in encrypted storage, and can be factory-reset with the BOOT button. It doesn't join Wi-Fi or talk to the hub yet: enrolment, polling, the real screens and signed updates follow. [docs/firmware-handoff.md](../docs/firmware-handoff.md) is the brief it is built to.

## The board

Waveshare **ESP32-S3-Touch-AMOLED-1.8**, **V2 only** (CO5300 screen, CST820 touch; the revision is marked on the back). ESP32-S3 with 16 MB of flash and 8 MB of PSRAM, 368 × 448 screen.

| | |
|---|---|
| `device_type` | `display-hub-waveshare-amoled-1.8` |
| `hardware_revision` | `waveshare-amoled-1.8-v2` |
| Version | 0.1.0, set by `PROJECT_VER` in [CMakeLists.txt](CMakeLists.txt) |

The revision is told at start-up from the touch chip that answers. A V1 board reports `waveshare-amoled-1.8-v1` and says on its screen that it isn't supported; a board whose touch chip doesn't answer reports `waveshare-amoled-1.8-unknown`. Neither is ever reported as a V2.

The board code is in [main/board/](main/board/). It uses the board package's screen, touch and brightness functions, but not its `bsp_display_start()`, which hands this QSPI screen to LVGL as if it were an RGB panel. It pulses the I/O expander's screen power and resets as Waveshare's examples do, and only reads the AXP2101 power chip.

## What it does

1. **Install it** with the desktop app's **Install firmware**, which erases the flash and writes the package.
2. **Waiting for setup.** The screen says to open Display Hub and choose **Add a display**. The firmware reads its USB port for as long as it runs and answers every `hello` and every `provision` ([docs/protocol-v1.md](../docs/protocol-v1.md), "Over USB").
3. **Set up.** Once a bundle is accepted and stored, the screen says so, with how many hub addresses it was given and whether it has Wi-Fi details. Nothing from the bundle itself is ever shown or logged. It stays set up across restarts, and refuses another bundle (`already_provisioned`) until it is factory-reset.
4. **Factory reset:** hold **BOOT** for 10 seconds (below).

What to answer over USB is decided by plain C in [components/dh_core](components/dh_core/README.md) (`dh_usb.h`), which the host tests run without a board. A bundle is settled in this order, and the first that applies is the answer:

| | Answer |
|---|---|
| Storage isn't working | `storage_failed` |
| Already set up | `already_provisioned`, changing nothing |
| The bundle breaks a rule | `invalid_bundle` |
| One of its firmware keys isn't a valid P-256 point | `invalid_bundle`, storing nothing |
| The display trusts a different set of firmware keys | `firmware_keys_locked` |
| A write fails | `storage_failed` |
| Otherwise | `provisioned` |

The setup is the last thing written, so a failed write never leaves a display that reads as set up.

### The USB port and the console

The console's log lines and the setup exchange share the chip's own USB port (USB-Serial-JTAG). The app ignores any line that doesn't start with `{`, so the two can share it as long as a log line never lands inside a reply. [main/usb_setup.c](main/usb_setup.c) makes sure of that: the console and the replies go through one transmit queue, a reply is handed to it whole, and only while the USB task holds the console's own lock, which every log line holds from its first character to its last. Each reply also starts on a line of its own. The top of that file has the detail, and what isn't covered (text the ROM prints directly, such as a crash report).

**The logging rule:** never log or show the Wi-Fi password, the enrolment secret, the device token, a line received over USB, or anything else from a bundle. Log facts that give nothing away: "bundle accepted and stored: 2 hub addresses, Wi-Fi set, firmware keys taken from the bundle". The buffers that held a bundle are wiped once it has been dealt with.

## Storage

Encrypted NVS, with ESP-IDF's HMAC scheme (the handoff's "[Storage and NVS](../docs/firmware-handoff.md#storage-and-nvs)" says why). Two namespaces, so a factory reset can erase one and leave the other:

| Namespace | Key | What |
|---|---|---|
| `dh_setup` | `setup` | The setup from the bundle: hub addresses, port, trust anchors, the enrolment secret and the Wi-Fi details, as one blob (`dh_setup_encode()`, at most 9464 bytes). The device ID, token and saved snapshot will go here too. |
| `dh_fw` | `keys` | The trusted firmware keys and the security floor (201 bytes). |
| `dh_fw` | `record` | The update record, with the attempt budget (321 bytes). |

Each value is one blob written in one go and read back, so a power cut leaves the old value or the new one. A read that fails is never treated as "nothing there": the display shows a storage problem and refuses setup. The partition is 64 KiB; the largest setup takes under three of its sixteen pages. [main/storage.c](main/storage.c) is all of it.

### The storage key

The encryption keys are worked out at each start by the chip's HMAC unit from a secret key in eFuse key block **KEY5**. eFuses are one-time memory. [main/storage_key.c](main/storage_key.c) looks at the eFuses before NVS is started:

- **KEY5 holds an HMAC_UP key:** encrypted NVS starts with it. Nothing is burnt.
- **KEY5 is empty** (purpose USER, all zeros, no protection), no other key block holds an HMAC_UP key, download mode and USB are allowed, Secure Boot and flash encryption are off, and no eFuse block reads back as damaged: this is **a new board's first start**. The firmware makes a random key with ESP-IDF's own function and **burns it into KEY5, permanently**, without asking. That uses KEY5 up for good, and is the intended behaviour: without it a new board could never be set up. The console says so in capitals, prints every key block and security setting before and after, and checks that nothing but KEY5's own bits changed. The firmware never sees the key again and never prints it.
- **Anything else** (KEY5 has another purpose or isn't empty, a block is damaged, any condition above fails): nothing is burnt and NVS isn't started. The screen shows **Storage problem** with the reasons, `hello` says `provisioned: false`, and a bundle is refused with `storage_failed`.

ESP-IDF's `nvs_flash_init()` would burn a key into an empty KEY5 whatever state the chip is in, so **this firmware never calls it**, and nothing added later may. It starts NVS with `nvs_flash_read_security_cfg_v2()` and `nvs_flash_secure_init()`, which can't burn anything. It never erases the NVS partition by itself either: if NVS won't start, **Install firmware** (which erases the flash) is the way out.

The scheme was checked on a real V2 board with an earlier build of this firmware: the burnt key reads back as all zeros (it can't be read out), what was stored isn't readable in the raw flash, and the board can still be erased and reflashed over USB, with the key outliving the erase.

What it protects, honestly: a plain read of the flash no longer shows the Wi-Fi password or the token. Someone with a cable who runs their own firmware on the chip can most likely still use its HMAC unit to decrypt.

## Factory reset

**Hold the BOOT button for 10 seconds while the display is running.** After the first second the screen counts down; letting go before the end cancels. (BOOT only puts the chip in its loader if it is held while the chip resets or is plugged in. Once the firmware is running it is an ordinary button.)

- **It erases** the `dh_setup` namespace: the hub addresses, port and trust anchors, the Wi-Fi details, the enrolment secret and, once there are any, the device ID, token and saved snapshot.
- **It keeps** the `dh_fw` namespace: the trusted firmware keys, the security floor and the update record. So a reset display can be paired with another hub, but still only accepts bundles naming the same firmware keys (or none): anything else is `firmware_keys_locked`, and only **Install firmware** changes the keys.
- The display goes straight back to **Waiting for setup**. It doesn't restart.

The button is debounced (it must read the same for 100 ms), a button already down at start-up does nothing until it has been let go, and one hold does one reset. The whole NVS partition is never erased.

## The console

Open the board's serial port at any speed (opening it may restart the board). At start-up, and again every minute, the firmware prints read-only facts: the version and the slot it runs from, the board's revision and parts, memory, every eFuse key block and security setting, whether it is set up (with the count of hub addresses and whether Wi-Fi is set, never their values), how many firmware keys it trusts, the security floor, the update attempts left, and how much of the main and USB tasks' stacks has never been used.

## Tests without a board

[host-tests/](host-tests/) builds `dh_core` for a computer and runs it against every protocol vector and example, with gcc and clang and with the sanitisers:

```sh
cmake -S firmware/host-tests -B /tmp/dh-host && cmake --build /tmp/dh-host && ctest --test-dir /tmp/dh-host --output-on-failure
```

For setup over USB that is `usb_tests.c` (every bundle rule, every refusal in order, failing writes, the reply lines against `protocol/v1/examples/usb`, the stored form) and the `provision-*` vectors run through the whole handler. [host-tests/usb-e2e/](host-tests/usb-e2e/README.md) goes further on Linux: the desktop app's real sender against the firmware's handler over a pseudo-terminal. [fuzz/](fuzz/README.md) has the fuzz targets.

## Building

You need **ESP-IDF v5.5.6** exactly, installed with Espressif's [instructions](https://docs.espressif.com/projects/esp-idf/en/v5.5.6/esp32s3/get-started/index.html) (for ESP32-S3). Other versions are refused by `main/idf_component.yml`. Then, from the repository's root folder:

```sh
. "$IDF_PATH/export.sh"
idf.py -C firmware -B firmware/build build
```

A build folder keeps the settings it was first made with. After changing `sdkconfig.defaults`, delete the build folder (or its `sdkconfig`) and build again.

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

There is no factory slot. Secure Boot and flash encryption are off. NVS encryption is on, with the HMAC scheme and eFuse key block KEY5 (see [The storage key](#the-storage-key)).
