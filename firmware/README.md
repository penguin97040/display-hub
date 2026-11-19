# Display Hub firmware

The official firmware for a Display Hub display, written in C with ESP-IDF. It is also the starter project for anyone making firmware for their own display.

**Status: early.** For now it is the **storage key check build**: the board test's screens, plus one that burns the storage key into the chip and checks encrypted storage on a real board (see [Storage key check](#storage-key-check)). Setup over USB, Wi-Fi, the hub protocol and signed updates follow. [docs/firmware-handoff.md](../docs/firmware-handoff.md) is the brief it is built to.

## The board

Waveshare **ESP32-S3-Touch-AMOLED-1.8**, **V2 only** (CO5300 screen, CST820 touch; the revision is marked on the back). ESP32-S3 with 16 MB of flash and 8 MB of PSRAM, 368 × 448 screen.

| | |
|---|---|
| `device_type` | `display-hub-waveshare-amoled-1.8` |
| `hardware_revision` | `waveshare-amoled-1.8-v2` |
| Version | 0.1.0, set by `PROJECT_VER` in [CMakeLists.txt](CMakeLists.txt) |

## Board test

Install it with the desktop app's **Install firmware**. It has four screens; swipe left or right to move between them (the dots at the bottom show which one you're on):

1. **About the board:** the firmware version, the app slot it is running from and its address (`ota_0` at `0x20000` after a first install), what the boot record says (`blank` after a first install), the board revision (V2 if the CST820 touch chip answers; a V1 board shows "not supported yet"), whether USB power is good, and free PSRAM and heap.
2. **Touch test:** a dot follows your finger, and the last swipe is shown in words.
3. **eFuse keys:** for each key block KEY0 to KEY5, its purpose, whether it is read-protected or write-protected, and whether it is empty; then whether download mode, secure download mode and USB-Serial-JTAG are turned off by eFuse, and whether Secure Boot and flash encryption are on. This screen only reads.
4. **Storage key:** see below.

The same facts, and more of the eFuse settings (JTAG, the raw protection bits and all of eFuse block 0), are printed to the console (USB-Serial-JTAG) every 10 seconds, and each swipe as it happens.

The board code is in [main/board/](main/board/). It uses the board package's screen, touch and brightness functions, but not its `bsp_display_start()`, which hands this QSPI screen to LVGL as if it were an RGB panel. It pulses the I/O expander's screen power and resets as Waveshare's examples do, and only reads the AXP2101 power chip.

## Storage key check

The firmware will keep the Wi-Fi password and the device token in encrypted storage (NVS), using ESP-IDF's HMAC scheme: a secret key lives in eFuse key block **KEY5**, and the chip's HMAC unit works the encryption keys out from it at each start. The handoff's "[Storage and NVS](../docs/firmware-handoff.md#storage-and-nvs)" says why. eFuses are one-time memory, so **putting the key there is permanent**: KEY5 is used up for good.

This build does that once, on a real board, and answers two things Espressif's pages don't say: whether the key can be read back out, and whether the board can still be erased and reflashed over USB afterwards.

### What it will and won't do

- **Nothing is burnt without two taps on the screen.** ESP-IDF's `nvs_flash_init()` would burn a key into an empty KEY5 without asking, so this build never calls it (it isn't even linked into the image). The one call that burns is in `storage_key_burn()` in [main/storage_key.c](main/storage_key.c), reached only from the confirmation.
- **The burn is only offered** when KEY5 is empty with the purpose USER and no protection, no other key block already holds an HMAC_UP key, download mode and USB are allowed, Secure Boot and flash encryption are off, and no eFuse block reads back as damaged. Otherwise the screen says why not.
- **The key is made by ESP-IDF's own code,** the same function production firmware will use, so it is made in exactly the same way: 32 bytes from the chip's random number generator with its noise source turned on. The firmware never sees the key again and never prints it.
- **Only KEY5's own bits change:** the key, its purpose (HMAC_UP), and its read- and write-protection. The console shows every key block and security setting before and after, and the build checks that nothing else moved.

### Running it

1. Install `display-hub-0.1.0-hmaccheck.dhfw` with **Install firmware**. If you want the console's record (worth having), open the serial console now and leave it open: opening it restarts the board, so don't open or close it, or unplug the cable, between the second tap and the result. Each restart adds one to the boot count, however it happens.
2. Swipe to the fourth screen. With an empty KEY5 it explains the burn and shows **Burn the key**.
3. Tap **Burn the key**, wait a second, then tap **Yes, burn it now**. (**Cancel** sits where the first button was, so a double tap cancels. Buttons ignore touches in their first second, swipes, and touches that slide on from elsewhere. Left alone for 30 seconds, or swiped away from, the second step goes back to the first.)
4. The result appears within a second or two: **Storage key: PASS** or **FAIL**, with a line for each check.
5. Tap **Restart**. The checks run again, and the boot count should be one higher.
6. Install the same package again with **Install firmware**. That erases and rewrites the whole flash. If the install works and the fourth screen says **PASS** again with the boot count back at 1, the burnt key doesn't get in the way of reflashing, and the key outlived the erase.

### The checks

They run at every start once KEY5 holds an HMAC_UP key, and are printed to the console every 10 seconds under `storage-key`.

| Line on the screen | What it means | A failure means |
|---|---|---|
| KEY5 purpose is HMAC_UP | The key block is marked for the HMAC unit. | The burn didn't finish. |
| KEY5 read- and write-protected | The key can't be read or changed, and its purpose is locked. | The key is exposed or could be altered. |
| Key readable: no | Reading the block as firmware would gives all zeros. | **Key readable: YES**: the key can be read out. Use plain NVS. |
| Encrypted storage started | The HMAC unit gave keys and encrypted NVS opened. | The scheme doesn't work on this board. |
| First start since an erase / Last start's data read back | What the last start stored was read back and is right. | Stored data was lost or can't be decrypted. |
| Boot count N (was N-1), saved | This start's count and marker were written and read back. | Writing encrypted NVS failed. |
| Marker not found in raw flash | The NVS partition, read exactly as it is in flash, doesn't hold the marker's text or its names. | **Marker FOUND**: the data isn't encrypted. |
| The burn changed only KEY5 | Shown on the start the key was burnt: nothing else in the eFuses changed. | Something else changed: compare the two eFuse reports in the console. |

If the key is readable, or the board can't be reflashed afterwards, the handoff says to ship plain NVS instead.

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

There is no factory slot. Secure Boot and flash encryption are off. NVS encryption is on, with the HMAC scheme and eFuse key block KEY5 (see [Storage key check](#storage-key-check)).
