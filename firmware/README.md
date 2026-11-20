# Display Hub firmware

The official firmware for a Display Hub display, written in C with ESP-IDF. It is also the starter project for anyone making firmware for their own display.

**Status: early.** It takes its setup over USB from the desktop app, keeps it in encrypted storage, joins Wi-Fi, enrols with the hub, polls for its pages, draws them, sends heartbeats, and can be factory-reset with the BOOT button. Installing signed updates follows. [docs/firmware-handoff.md](../docs/firmware-handoff.md) is the brief it is built to.

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
3. **Set up.** Once a bundle is accepted and stored, the display joins the Wi-Fi network in it and enrols with the hub, once, with the bundle's one-time secret. Nothing from the bundle itself is ever shown or logged. It stays set up across restarts, and refuses another bundle (`already_provisioned`) until it is factory-reset.
4. **Running.** It asks the hub for its snapshot as often as the hub says, keeps the last good one, and sends a heartbeat after start-up, hourly, and when its update attempts left change. The screen shows the pages, one at a time ([The screen](#the-screen)). When something is wrong it keeps the last pages and says what, in plain words ([Talking to the hub](#talking-to-the-hub)).
5. **Factory reset:** hold **BOOT** for 10 seconds (below).

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

**The logging rule:** never log or show the Wi-Fi password, the enrolment secret, the device token, a line received over USB, or anything else from a bundle: not the network's name, and not a hub address. Log facts that give nothing away: "bundle accepted and stored: 2 hub addresses, Wi-Fi set, firmware keys taken from the bundle", "hub address 1 of 2, certificate 1 of 1: no answer". ESP-IDF's own log lines that would name the network, the display's address or a hub address are turned down for the same reason (`wifi`, `esp_netif_handlers` and `esp-tls`). The buffers that held a bundle, a secret or the token are wiped once they have been dealt with.

## Talking to the hub

[main/hub.c](main/hub.c) is one task that does everything on the network, with [main/wifi.c](main/wifi.c) for the Wi-Fi link and [main/hub_http.c](main/hub_http.c) for one HTTPS request. What to send, what an answer means and when to try next are decided by plain C in `dh_core` (`dh_hub.h`, `dh_snapshot.h`, `dh_poll.h`), which the host tests run without a board.

**Wi-Fi.** A station only, never an access point. The network's name and password go from the encrypted setup to the Wi-Fi driver at each start, and the driver is told to keep them in memory only. With a password set, the display never joins a network of that name that asks for none. A join that fails is tried again after 15 seconds, 30 seconds, then every minute (each ±20%); a link that drops is tried again at once.

**TLS**, the same for every request, as the handoff's "TLS, exactly" says:

- the bundle's certificate is the only trust anchor (`cert_pem`). No bundle of public authorities is built in (`CONFIG_MBEDTLS_CERTIFICATE_BUNDLE` is off), and the global store isn't used;
- the name checked is always `hub.display-hub.internal` (`common_name`), whatever address is connected to, and the check is never skipped;
- TLS 1.2 and 1.3;
- **no redirect is ever followed**: the firmware never calls `esp_http_client_perform()`, the only place ESP-IDF follows one, so that code isn't even in the image. A redirect reads as an error;
- no request goes anywhere but the hub. There is no SNTP: ages are worked out on the hub's clock, from the snapshot's own times and how long it has been held.

**Which address.** The hub addresses are tried in the bundle's order. An address that gives no answer is left for the next. At one that answers with a certificate that doesn't match, the second trust anchor is tried, if the bundle had two. The first answer from the real hub ends it, whether it is a success or an error. If none answers, the screen says what the last address did, as the reference display does.

**`.local` names.** `raspberrypi.local` and the like are looked up by lwIP's own resolver, which sends a one-shot mDNS query for any name ending in `.local` (`CONFIG_LWIP_DNS_SUPPORT_MDNS_QUERIES`, on by default and set in `sdkconfig.defaults`). `esp_http_client` looks names up through esp-tls with `getaddrinfo()`, which is that resolver. That is from reading ESP-IDF v5.5.6's source, and **hasn't been seen working on a board yet**. The certificate check, not the name, proves it is the hub; a bundle should carry the hub's fixed address as well.

**Limits.** A snapshot over 32 KiB is refused before it is parsed: if the hub says it is longer, none of it is read. An enrolment response or an error answer is read to 8 KiB at most. A connection, and each wait for more of an answer, times out after 10 seconds.

**Enrolment, and the order of the two writes.** The hub's answer holds the device token, and the one-time secret is used up from then on. The token is written first, read back, and only then is the secret taken out of the stored setup. A power cut between the two leaves a display with both, which the next start tidies up. The other order could leave a display that the hub has enrolled with neither a secret nor a token. If the hub refuses the secret (`enrolment_used` or `enrolment_expired`), the secret is erased and the screen says the display needs setting up again. (A power cut after the hub has answered but before the token is in flash can't be recovered: the secret is spent. The display then says it needs setting up again.)

**The last good snapshot** is saved as the hub sent it, and shown after a restart as "saved; age unknown" until a poll succeeds. A snapshot's `generated_at` moves with every poll, so the saved copy is only replaced when something else in it differs, and then at most once an hour: a restart shows pages that may be an hour old for a few seconds. That keeps the flash's wear to a few writes a day.

**Errors** follow the handoff's table, and the screen uses its words: can't reach the hub; something else is answering as the hub (a certificate that doesn't match); the hub sent data this display can't read; the hub is busy; this display needs setting up again; hub and display versions don't match. After a failure it waits 15 seconds, doubling to 15 minutes (±20%), or an hour for the last two.

**Firmware offers** in a snapshot are read and left alone for now. `dh_fw_tick()` runs every minute, so running time earns update attempts back as the rules say.

## The screen

[main/ui.c](main/ui.c) decides what shows, [main/ui_page.c](main/ui_page.c) draws a page, and the words and figures come from plain C in [main/ui_text.c](main/ui_text.c), which the host tests run without a board. The wording follows the desktop app's preview of a page.

- **One page at a time, in the hub's order.** Swipe left for the next, right for the one before; the row doesn't wrap round. Dots along the bottom say where you are. After the last page comes the **status screen**: how things stand with the hub and, when something is wrong, what to do about it. When new pages arrive, the page being looked at stays on screen by its `id`, wherever it has moved to; if it has gone, its nearest neighbour that is still there takes its place. With no pages, the screen says "No pages yet" and to choose this display's pages in Display Hub.
- **The four shapes.** A meter has a line for each bar (1 to 6) with its share, or "unknown" and an empty bar for a share that isn't known (never 0%), and "Resets in 3 h 20 min" or "Reset due" if it has a reset time. A big number is drawn as large as fits on one line, with its decimal places and unit, a dash when it isn't known (never 0), and its caption; the widest figure the hub can send wraps rather than being cut off. A text page is its paragraph; a list is its lines, each with its detail at the end.
- **What can't be drawn** gets the handoff's card: the title, "This display can't show this page." and, for a kind this firmware doesn't know, "It may need a firmware update." Nothing else about the page. An unavailable page with no data shows its title, its note (or "No data available") and UNAVAILABLE, with no figures.
- **Every other page** shows its data quality (DEMO, REPORTED, ESTIMATE, MANUAL or UNAVAILABLE; one from a newer hub reads as UNAVAILABLE), its note, and its age: "Updated 10 min ago", or "Stale. Updated 2 d ago" in amber. Ages and countdowns are on the hub's clock (the snapshot's `generated_at` and the page's `updated_at`, plus how long the snapshot has been held by `esp_timer`), never the display's own date, and they are brought up to date every 30 seconds. Pages restored from flash after a restart say "Saved; age unknown", and show no countdowns, until a poll succeeds.
- **Problems** show as one short coloured line at the foot of every page (the headlines in [Talking to the hub](#talking-to-the-hub), and "Can't join Wi-Fi"), and the status screen's dot takes the same colour. Without pages, or before the display is set up, or with a storage problem, the status screen is all there is. The factory reset countdown always covers everything.
- **The font** is Geist, as in the desktop app: its Regular and SemiBold TrueType files are built into the app and drawn at any size by LVGL's built-in Tiny TTF (stb_truetype); each glyph is drawn once and kept in a cache. Titles, labels and notes can hold any Unicode: Geist covers Latin with its accents (macrons included) and basic Cyrillic, and anything it lacks is drawn as a hollow box without upsetting the rest of the line. LVGL takes its memory from PSRAM first ([main/ui_font.c](main/ui_font.c)), so the widgets and glyph caches leave the internal memory to Wi-Fi and TLS.
- **Burn-in care.** Everything is white or grey on black (a black AMOLED pixel is off), and every 3 minutes the whole picture moves a few pixels, round a small square and back, so nothing stays lit in one place. Everything is laid out far enough in from the edges that this never cuts anything off.
- **Threads.** The hub task never draws. It keeps a copy of its pages for the screen, and the main task copies them (about 38 KB, into PSRAM) only when they have changed, while holding the LVGL lock. Swipes and the 30-second refresh run in LVGL's own task, which holds the same lock.

## Storage

Encrypted NVS, with ESP-IDF's HMAC scheme (the handoff's "[Storage and NVS](../docs/firmware-handoff.md#storage-and-nvs)" says why). Two namespaces, so a factory reset can erase one and leave the other:

| Namespace | Key | What |
|---|---|---|
| `dh_setup` | `setup` | The setup from the bundle: hub addresses, port, trust anchors, the enrolment secret (until it is used) and the Wi-Fi details, as one blob (`dh_setup_encode()`, at most 9464 bytes). |
| `dh_setup` | `device` | The device ID and token the hub gave it (`dh_device_encode()`, at most 195 bytes). |
| `dh_setup` | `snapshot` | The last good snapshot, as the hub sent it (at most 32 KiB). |
| `dh_fw` | `keys` | The trusted firmware keys and the security floor (201 bytes). |
| `dh_fw` | `record` | The update record, with the attempt budget (321 bytes). |

Each value is one blob written in one go and read back, so a power cut leaves the old value or the new one. A read that fails is never treated as "nothing there": the display shows a storage problem and refuses setup. [main/storage.c](main/storage.c) is all of it.

The partition is 64 KiB, and the largest of everything fits in it at once with room to spare. What doesn't fit is two large snapshots: NVS writes a blob's new value before it lets go of the old one, so replacing one 32 KiB snapshot with another in one step fails for want of room (leaving the old one whole). So when the old and new snapshots together are over 40,000 bytes, the old one is erased first; a power cut between the two steps leaves no saved snapshot, which only costs the pages shown before the first poll. Smaller ones, and every other value, are replaced in one step. The top of `storage.c` has the figures, and how this was tried against ESP-IDF's NVS code on a computer.

ESP-IDF's radio code also keeps its calibration data in this partition, in a namespace of its own (`phy`). It holds nothing from a bundle, and a factory reset leaves it. The Wi-Fi driver's own storage is turned off (`CONFIG_ESP_WIFI_NVS_ENABLED`), so the network's name and password are never written anywhere but `dh_setup`.

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

- **It erases** the `dh_setup` namespace: the hub addresses, port and trust anchors, the Wi-Fi details, the enrolment secret, the device ID and token, and the saved snapshot.
- **It keeps** the `dh_fw` namespace: the trusted firmware keys, the security floor and the update record. So a reset display can be paired with another hub, but still only accepts bundles naming the same firmware keys (or none): anything else is `firmware_keys_locked`, and only **Install firmware** changes the keys.
- The display goes straight back to **Waiting for setup**. It doesn't restart.
- **It never waits for the network.** The erase happens at once, whatever the display is doing. The task that talks to the hub works from its own copy of the setup; it finds the setup gone, gives up any request within one network wait (about ten seconds at most), leaves Wi-Fi and wipes the token and everything else it held. Every write it makes names the setup it belongs to, so nothing in flight can be stored after a reset, or beside a new setup.
- The hub isn't told. Remove the display in the desktop app as well, or it stays in the list.

The button is debounced (it must read the same for 100 ms), a button already down at start-up does nothing until it has been let go, and one hold does one reset. The whole NVS partition is never erased.

## The console

Open the board's serial port at any speed (opening it may restart the board). At start-up, and again every minute, the firmware prints read-only facts: the version and the slot it runs from, the board's revision and parts, memory, every eFuse key block and security setting, whether it is set up (with the count of hub addresses, whether Wi-Fi is set, whether a device token is held and the size of the saved snapshot, never their values), how many firmware keys it trusts, the security floor, the update attempts left, and how much of the main, USB and hub tasks' stacks has never been used. The hub task logs each thing it does: joining Wi-Fi, enrolling (with the device ID the hub gave it, never the token), each poll with its page count and when the next is due, each heartbeat, and each failure with the status and error code.

## Tests without a board

[host-tests/](host-tests/) builds `dh_core` for a computer and runs it against every protocol vector and example, with gcc and clang and with the sanitisers:

```sh
cmake -S firmware/host-tests -B /tmp/dh-host && cmake --build /tmp/dh-host && ctest --test-dir /tmp/dh-host --output-on-failure
```

For the screen it is `ui_tests.c` (ages and countdowns on the hub's clock, saved pages, shares and big numbers with every edge of their range, how each page is drawn, which page is kept when the pages change, and the burn-in shift). For talking to the hub it is `hub_tests.c` (the enrolment request and the heartbeat, the enrolment response and every error group, the stored device ID and token, which address and anchor to try next, when a heartbeat is due and when a snapshot is saved). For setup over USB it is `usb_tests.c` (every bundle rule, every refusal in order, failing writes, the reply lines against `protocol/v1/examples/usb`, the stored form) and the `provision-*` vectors run through the whole handler. [host-tests/usb-e2e/](host-tests/usb-e2e/README.md) goes further on Linux: the desktop app's real sender against the firmware's handler over a pseudo-terminal. [fuzz/](fuzz/README.md) has the fuzz targets.

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
