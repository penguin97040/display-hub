# Display Hub firmware

The official firmware for a Display Hub display, written in C with ESP-IDF. It is also the starter project for anyone making firmware for their own display.

**Status: working, and tested on real V2 boards** from a first install to signed updates with rollback. It takes its setup over USB from the desktop app, keeps it in encrypted storage, joins Wi-Fi, enrols with the hub, polls for its pages, draws them, sends heartbeats, installs the signed updates the hub offers (going back by itself if a new version can't reach the hub), and can be factory-reset with the BOOT button. [docs/firmware-handoff.md](../docs/firmware-handoff.md) is the brief it is built to.

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
2. **Waiting for setup.** The screen says to open Display Hub and choose **Add a display**. The firmware reads its USB port for as long as it runs and answers every `hello` and every `provision` ([docs/protocol-v1.md](../docs/protocol-v1.md), "Over USB"). A new board has no storage key yet, and nothing is burnt while it waits.
3. **Set up.** A new board's first bundle, once it has passed every check, is when its storage key is made ([The storage key](#the-storage-key)). Once a bundle is accepted and stored, the display joins the Wi-Fi network in it and enrols with the hub, once, with the bundle's one-time secret. Nothing from the bundle itself is ever shown or logged. It stays set up across restarts, and refuses another bundle (`already_provisioned`) until it is factory-reset.
4. **Running.** It asks the hub for its snapshot as often as the hub says, keeps the last good one, and sends a heartbeat after start-up, hourly, and when its update attempts left change. The screen shows the pages, one at a time ([The screen](#the-screen)). When something is wrong it keeps the last pages and says what, in plain words ([Talking to the hub](#talking-to-the-hub)).
5. **Updates.** When the owner presses **Update** in the desktop app, the display downloads the package through the hub, checks it against the keys it already trusts, and restarts into it on trial ([Signed updates](#signed-updates)).
6. **Factory reset:** hold **BOOT** for 10 seconds (below).

What to answer over USB is decided by plain C in [components/dh_core](components/dh_core/README.md) (`dh_usb.h`), which the host tests run without a board. A bundle is settled in this order, and the first that applies is the answer:

| | Answer |
|---|---|
| Storage isn't working | `storage_failed` |
| Already set up | `already_provisioned`, changing nothing |
| The bundle breaks a rule | `invalid_bundle` |
| One of its firmware keys isn't a valid P-256 point | `invalid_bundle`, storing nothing |
| The display trusts a different set of firmware keys | `firmware_keys_locked` |
| A new board's storage key can't be made, or a write fails | `storage_failed` |
| Otherwise | `provisioned` |

The setup is the last thing written, so a failed write never leaves a display that reads as set up.

### The USB port and the console

The console's log lines and the setup exchange share the chip's own USB port (USB-Serial-JTAG). The app ignores any line that doesn't start with `{`, so the two can share it as long as a log line never lands inside a reply. [main/usb_setup.c](main/usb_setup.c) makes sure of that: the console and the replies go through one transmit queue, a reply is handed to it whole, and only while the USB task holds the console's own lock, which every log line holds from its first character to its last. Each reply also starts on a line of its own. The top of that file has the detail, and what isn't covered (text the ROM prints directly, such as a crash report).

**The logging rule:** never log or show the Wi-Fi password, the enrolment secret, the device token, a line received over USB, or anything else from a bundle: not the network's name, and not a hub address. Log facts that give nothing away: "bundle accepted and stored: 2 hub addresses, Wi-Fi set, firmware keys taken from the bundle", "hub address 1 of 2, certificate 1 of 1: no answer". ESP-IDF's own log lines that would name the network, the display's address or a hub address are turned down for the same reason (`wifi`, `esp_netif_handlers` and `esp-tls`). The firmware's own buffers that held a bundle, a secret or the token are wiped once they have been dealt with. Two copies of the token are out of its reach and are not: ESP-IDF's HTTP client frees its receive buffer (which the enrolment answer passes through) and its request headers (which carry the token in every later request) without wiping them, so they sit in freed memory until it is used again. The token is in memory for as long as the display is enrolled anyway.

## Talking to the hub

[main/hub.c](main/hub.c) is one task that does everything on the network, with [main/wifi.c](main/wifi.c) for the Wi-Fi link and [main/hub_http.c](main/hub_http.c) for one HTTPS request. What to send, what an answer means and when to try next are decided by plain C in `dh_core` (`dh_hub.h`, `dh_snapshot.h`, `dh_poll.h`), which the host tests run without a board.

**Wi-Fi.** A station only, never an access point. The network's name and password go from the encrypted setup to the Wi-Fi driver at each start, and the driver is told to keep them in memory only. With a password set, the display never joins a network of that name that asks for none. A join that fails is tried again after 15 seconds, 30 seconds, then every minute (each ±20%); a link that drops is tried again at once. After a failed join the console says why, in words, and (after the first few failures, then every tenth) lists the access points it can see for the network, with their channel and signal. A board that sees its network but never joins (every join failing with reason 2, `AUTH_EXPIRE`) may have a faulty radio: one such board failed the same way with Espressif's plain station example. Try another board.

**TLS**, the same for every request, as the handoff's "TLS, exactly" says:

- the bundle's certificate is the only trust anchor (`cert_pem`). No bundle of public authorities is built in (`CONFIG_MBEDTLS_CERTIFICATE_BUNDLE` is off), and the global store isn't used;
- the name checked is always `hub.display-hub.internal` (`common_name`), whatever address is connected to, and the check is never skipped;
- TLS 1.2 and 1.3;
- **no redirect is ever followed**: the firmware never calls `esp_http_client_perform()`, the only place ESP-IDF follows one, so that code isn't even in the image. A redirect reads as an error;
- no request goes anywhere but the hub. There is no SNTP: ages are worked out on the hub's clock, from the snapshot's own times and how long it has been held.

**Which address.** The address that answered last is tried first, then the others in the bundle's order. An address that gives no answer is left for the next. At one that answers with a certificate that doesn't match, the second trust anchor is tried, if the bundle had two. The first answer from the real hub ends it, whether it is a success or an error. If none answers, the screen says what the last address tried did, as the reference display does. Which address answered last is kept in memory only: every start, and every new setup, begins with the bundle's order.

**`.local` names.** `raspberrypi.local` and the like are looked up by lwIP's own resolver, which sends a one-shot mDNS query for any name ending in `.local` (`CONFIG_LWIP_DNS_SUPPORT_MDNS_QUERIES`, on by default and set in `sdkconfig.defaults`). `esp_http_client` looks names up through esp-tls with `getaddrinfo()`, which is that resolver. On the one network it has been tried on, the name was never found (the lookup gave up after about 7 seconds, every time) and the fixed address worked. That is why the address that answered last goes first: without it every request waited those 7 seconds. The certificate check, not the name, proves it is the hub; a bundle should carry the hub's fixed address as well.

**Limits.** A snapshot over 32 KiB is refused before it is parsed: if the hub says it is longer, none of it is read. An enrolment response or an error answer is read to 8 KiB at most. The TCP connection, each write and each wait for more of the answer's headers time out after 10 seconds. And **the whole request has 30 seconds from its start**: the connection, the TLS handshake, sending the request, and the answer's headers and body, whichever of them is dragged out and whoever is at the other end. That matters most for the handshake, which anything that answers at a hub address can take part in, with or without the hub's key. ESP-IDF's built-in connection can't keep such a limit (its handshake runs on a blocking socket, and its header reading only stops when the headers end), so [main/hub_http.c](main/hub_http.c) hands the HTTP client a connection of its own, made with esp-tls on a socket that never blocks: it looks at the limit, and at whether a factory reset has happened, before every read and write and at least every 50 ms while it waits. The body is read 64 bytes at a time with a 50 ms wait, so a hub that trickles bytes can overrun the 30 seconds by 3.2 seconds at most. Two waits can't be cut short, and nothing that answers can stretch either: the name lookup (lwIP's own, a few seconds) and the wait for the TCP connection (10 seconds). A download's headers have the same 30 seconds, and its body has its own limits ([Signed updates](#signed-updates)). The top of `hub_http.c` has the ESP-IDF lines this rests on, and how it was tried on a computer against a stand-in hub that trickles, floods and goes quiet at each stage.

**Enrolment, and the order of the two writes.** The hub's answer holds the device token, and the one-time secret is used up from then on. The token is written first, read back, and only then is the secret taken out of the stored setup. A power cut between the two leaves a display with both, which the next start tidies up. The other order could leave a display that the hub has enrolled with neither a secret nor a token. If the hub refuses the secret (`enrolment_used` or `enrolment_expired`), the secret is erased and the screen says the display needs setting up again. (A power cut after the hub has answered but before the token is in flash can't be recovered: the secret is spent. The display then says it needs setting up again.)

**The last good snapshot** is saved as the hub sent it, and shown after a restart as "saved; age unknown" until a poll succeeds. A snapshot's `generated_at` moves with every poll, so the saved copy is only replaced when something else in it differs, and then at most once an hour: a restart shows pages that may be an hour old for a few seconds. That keeps the flash's wear to a few writes a day.

**Errors** follow the handoff's table, and the screen uses its words: can't reach the hub; something else is answering as the hub (a certificate that doesn't match); the hub sent data this display can't read; the hub is busy; this display needs setting up again; hub and display versions don't match. After a failure it waits 15 seconds, doubling to 15 minutes (±20%), or an hour for the last two.

## Signed updates

[docs/firmware-updates.md](../docs/firmware-updates.md) is the design, and "What a display must do" in it is what this firmware does. What is genuine, what fits this display and when to give up are decided by plain C in `dh_core` (`dh_update.h`), which the host tests run against every firmware vector. The firmware's own part is three files:

- [main/hub.c](main/hub.c) reports, and asks the hub for the update, on the task that does everything else on the network;
- [main/update.c](main/update.c) reads the download, writes the spare app partition and runs the trial boot's clock;
- [main/storage_fw.c](main/storage_fw.c) holds the trusted keys, the security floor and the update record, and makes every save in the order the update core asks for.

**One update, in order.** After each poll the hub answered, the display sends any final report that is still waiting, then looks at the offer in the snapshot.

1. The update core decides (`dh_fw_on_offer`). An offer it would refuse from the offer alone (no trusted keys, too large) is refused and reported, and costs nothing. With no attempts left, nothing is requested and nothing reported.
2. Otherwise one attempt is spent, and the reduced budget and the offer id are **saved in one write before anything is requested**. If that write fails, nothing is requested.
3. `downloading` is reported, once.
4. `GET /v1/device/firmware/{package_id}`, with the same pinned TLS, token and walk of addresses as every other request. Only a `200` with an exact `Content-Length` equal to the offer's size is read at all; a chunked answer, or one of another length, is refused before a byte of its body is read.
5. The body is read a little at a time (at most 256 bytes a read, gathered into pieces of up to 4 KiB) and handed to the update core, which checks the header, the key against the display's own stored copy, the signature, and then the manifest. **Nothing is written to flash before all of that has passed**, and no byte past the update form is ever asked for.
6. Only then is the spare partition opened (`esp_ota_begin`) and the image written as it arrives, with a running SHA-256. A hash that doesn't match abandons the partition (`esp_ota_abort`).
7. `esp_ota_end` checks the image is a valid app. The record is saved (the release waiting for its trial boot), **then** the spare partition is set to boot (`esp_ota_set_boot_partition`), then the display restarts. If setting it to boot fails, the firmware reads back which partition the bootloader will start: if it is still the running one, the update ends there as `failed` with `flash` and the display doesn't restart; if it is the new one after all, that is an install; and if it can't tell, it restarts with the record kept, so whichever firmware starts settles it.

**An image that was given up can't be started.** `esp_ota_abort` only lets go of the partition: what was written stays, and a download that failed its hash can be a whole, valid app that nobody signed. It is never set to boot, but the bootloader falls back by itself to any app that passes its own check if the running one is ever damaged (Secure Boot is off). So whenever an update ends without a restart after a write to the spare partition was tried (a hash that doesn't match, a download that broke off, a failed write, a record that couldn't be saved, a partition that couldn't be set to boot), the firmware erases the first sector of the spare partition, which holds the image's header, and reads it back as blank. It never erases the partition it is running from, or the one the bootloader is set to start next, and an offer refused, or a download that broke off, before the first write leaves the spare partition as it was: opening it erases nothing, so it still holds the version before.

A download that sends nothing for 30 seconds, or takes 15 minutes in all, is given up; both are measured with `esp_timer_get_time()`, which only counts up from the start. A refusal or a failure is saved, then reported. The final report is settled by any `2xx` answer (a `204` for an offer already settled included), by `404` with `not_found` (the hub has no such offer) and by `token_revoked` or `unknown_device` with a 4xx status (the hub no longer knows the display). After anything else it is sent again, once after each later poll and never in a loop: no answer, a `5xx`, `rate_limited`, the faults the protocol says to retry (`bad_request`, `unauthorised` and the like), a code this firmware doesn't know, or an answer with no error body. While a final report waits, no new offer is acted on, so the waiting is bounded: once the hub has refused the same report 10 times, the display gives it up, logs that it did, and treats it as settled. Only a lasting refusal counts: a 4xx answer with a fault code, an unknown code or no error body. A hub that is busy or away never counts (no answer, any 5xx, 408, 429, `rate_limited`, `unavailable`), so an outage of any length costs no report. The count starts again at a restart. (The reference display counts any answer at all as delivered.)

**How long one read can take.** `esp_http_client_read()` keeps reading until it has all it was asked for, and can't be cut short from outside, so a hub that sends a byte at a time could hold one call open for (bytes asked for) × (the read timeout). Underneath it, Mbed TLS would wait on a blocking socket for a whole TLS record however slowly its bytes came, so the connection's socket never blocks ([Talking to the hub](#talking-to-the-hub), "Limits"): a record that isn't all there yet reads as "nothing yet". Each call then asks for at most 256 bytes with a 50 ms timeout: 12.8 seconds at the very worst. That is the most a download can overrun the 30 seconds, the 15 minutes or a factory reset, and only against a hub that trickles on purpose. A hub that just goes quiet is noticed within 50 ms.

**When a write to storage fails.** A write of the keys or the record that is reported as failed may still have reached flash, so nobody knows which record flash then holds. The firmware doesn't guess and doesn't try again: all update work stops until the display restarts (no offers, no saves, no update reports; the log and the status screen say so), while the pages, polls and heartbeats carry on. Until then enrolments, heartbeats and the USB `hello` leave the `firmware` member out, so the hub isn't told keys or attempts that may not match flash, and a bundle over USB is refused with `storage_failed` before anything is written, with a message that says to restart the display and try again (the screen says the same, as soon as the write fails, whichever task it failed on): it would write the keys and the record from memory over whatever flash holds. The next start reads flash and settles whatever is there. The record is never erased to tidy up, because a missing record reads as a new one with a full attempt budget.

**The trial boot.** The bootloader starts new firmware once on trial (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`). On that start the firmware sends nothing that names its version: no enrolment, no heartbeat, no report, and it acts on no offer. Its check-in is its ordinary poll, `GET /v1/device/snapshot`, tried at least every 15 seconds. As soon as the hub answers with success, or with `token_revoked` or `unknown_device` (which going back wouldn't fix), the image is marked good (`esp_ota_mark_app_valid_cancel_rollback`), then the release's security floor and keys are saved, then the record, and only then is `installed` reported. If 300 seconds of running pass first, a small task of its own (so it depends on nothing else working) calls `esp_ota_mark_app_invalid_rollback_and_reboot()` and saves nothing. A crash or a power cut on the trial boot does the same through the bootloader. The previous firmware, starting again, finds the unfinished record, drops the release's keys, leaves the floor alone and reports `rolled_back`. If there turns out to be nothing to go back to (no other image passes its check), the new firmware carries on, and because it is the only one that works, the next answer from the hub marks it good however late it comes.

"On trial" fails closed. If the update record says a release is waiting for its trial boot in the partition that is running, the start counts as marked good only when the bootloader's record of the image reads back as valid. Anything else, a record that can't be read included, is a trial boot: the release's security floor and keys are never applied without a check-in.

**The attempt budget** (five attempts, one earned back for each four hours of running) is counted by `dh_fw_tick()` every minute and whenever an offer is looked at, and a heartbeat goes as soon as the attempts left change.

**Flash and memory during a download.** The spare partition is opened with `OTA_WITH_SEQUENTIAL_WRITES`, so each 4 KiB sector is erased just before it is written: no single call stalls for seconds, and a download that fails early has worn only the sectors it reached. While a sector is erased or written (about 45 ms each) the chip can't run code from flash, so the screen's task and touch pause and run in the gaps: the picture may stutter during the download, and nothing is lost, because the screen's draw buffers are in internal memory and the panel keeps its own picture. The piece being written is in internal memory too, because ESP-IDF writes a buffer in PSRAM 32 bytes at a time. Only one TLS session is ever open: that is why `downloading` is reported before the download is asked for, where the reference display reports it once the manifest has passed. The top of `main/update.c` has what was read in ESP-IDF to decide each of these.

**The screen** says "Updating its firmware" at the foot of each page while it downloads, and keeps showing the pages until the restart. The status screen says the same in full, says when new firmware is on trial, and says how the last update ended (installed, went back, didn't finish, refused, including an offer refused before anything was requested). The firmware's version is at the foot of the status screen.

## The screen

[main/ui.c](main/ui.c) decides what shows, [main/ui_page.c](main/ui_page.c) draws a page, and the words and figures come from plain C in [main/ui_text.c](main/ui_text.c), which the host tests run without a board. The wording follows the desktop app's preview of a page.

- **One page at a time, in the hub's order.** Swipe left for the next, right for the one before; the row doesn't wrap round. Dots along the bottom say where you are. After the last page comes the **status screen**: how things stand with the hub and, when something is wrong, what to do about it. When new pages arrive, the page being looked at stays on screen by its `id`, wherever it has moved to; if it has gone, its nearest neighbour that is still there takes its place. With no pages, the screen says "No pages yet" and to choose this display's pages in Display Hub.
- **The four shapes.** A meter has a line for each bar (1 to 6) with its share, or "unknown" and an empty bar for a share that isn't known (never 0%), and "Resets in 3 h 20 min" or "Reset due" if it has a reset time. A big number is drawn as large as fits on one line, with its decimal places and unit, a dash when it isn't known (never 0), and its caption; the widest figure the hub can send wraps rather than being cut off. A text page is its paragraph; a list is its lines, each with its detail at the end.
- **What can't be drawn** gets the handoff's card: the title, "This display can't show this page." and, for a kind this firmware doesn't know, "It may need a firmware update." Nothing else about the page. An unavailable page with no data shows its title, its note (or "No data available") and UNAVAILABLE, with no figures.
- **Every other page** shows its data quality (DEMO, REPORTED, ESTIMATE, MANUAL or UNAVAILABLE; one from a newer hub reads as UNAVAILABLE), its note, and its age: "Updated 10 min ago", or "Stale. Updated 2 d ago" in amber. Ages and countdowns are on the hub's clock (the snapshot's `generated_at` and the page's `updated_at`, plus how long the snapshot has been held by `esp_timer`), never the display's own date, and they are brought up to date every 30 seconds (so under a minute they say "Updated just now" and "Resets in under a minute", where the desktop app counts the seconds). Pages restored from flash after a restart say "Saved; age unknown", and show no countdowns, until a poll succeeds.
- **Problems** show as one short coloured line at the foot of every page (the headlines in [Talking to the hub](#talking-to-the-hub), and "Can't join Wi-Fi"), and the status screen's dot takes the same colour. Without pages, or before the display is set up, or with a storage problem, the status screen is all there is. The factory reset countdown always covers everything.
- **The font** is Geist, as in the desktop app: its Regular and SemiBold TrueType files are built into the app and drawn at any size by LVGL's built-in Tiny TTF (stb_truetype); each glyph is drawn once and kept in a cache. Titles, labels and notes can hold any Unicode: Geist covers Latin with its accents (macrons included) and basic Cyrillic, and anything it lacks is drawn as a hollow box without upsetting the rest of the line. LVGL takes its memory from PSRAM first ([main/ui_font.c](main/ui_font.c)), so the widgets and glyph caches leave the internal memory to Wi-Fi and TLS.
- **Burn-in care.** Everything is white or grey on black (a black AMOLED pixel is off), and every 3 minutes the whole picture moves a few pixels, round a small square and back, so nothing stays lit in one place. Everything is laid out far enough in from the edges that this never cuts anything off.
- **Threads.** The hub task never draws. It keeps a copy of its pages for the screen, and the main task copies them (about 38 KB, into PSRAM) only when they have changed, while holding the LVGL lock. Swipes and the 30-second refresh run in LVGL's own task, which holds the same lock.

## Storage

Encrypted NVS, with ESP-IDF's HMAC scheme (the handoff's "[Storage and NVS](../docs/firmware-handoff.md#storage-and-nvs)" says why). Two NVS partitions, both encrypted with the same storage key, so a factory reset can erase one of them, every sector of it, and leave the other:

| Partition | Namespace | Key | What |
|---|---|---|---|
| `nvs_setup` | `dh_setup` | `setup` | The setup from the bundle: hub addresses, port, trust anchors, the enrolment secret (until it is used) and the Wi-Fi details, as one blob (`dh_setup_encode()`, at most 9464 bytes). |
| `nvs_setup` | `dh_setup` | `device` | The device ID and token the hub gave it (`dh_device_encode()`, at most 195 bytes). |
| `nvs_setup` | `dh_setup` | `snapshot` | The last good snapshot, as the hub sent it (at most 32 KiB). |
| `nvs` | `dh_fw` | `keys` | The trusted firmware keys and the security floor (201 bytes). |
| `nvs` | `dh_fw` | `record` | The update record, with the attempt budget (321 bytes). |
| `nvs` | `dh_reset` | `wiping` | There only while a factory reset's erase is under way ([Factory reset](#factory-reset)). |

Each value is one blob written in one go and read back, so a power cut leaves the old value or the new one. A read that fails is never treated as "nothing there": the display shows a storage problem and refuses setup. [main/storage.c](main/storage.c) is all of it, except that the keys, the record and the saves an update makes are in [main/storage_fw.c](main/storage_fw.c).

**Why a partition of its own for the setup.** Erasing a key or a namespace in NVS only marks its entries as erased; the bytes stay in flash until NVS one day reclaims the page. They are encrypted, but flash encryption is off, so someone holding the board could run their own program on the chip and have its HMAC unit work the NVS keys out. So what a factory reset must remove has a partition to itself, and the reset erases the flash under it.

`nvs_setup` is 128 KiB, and the largest of everything fits in it with room for a second 32 KiB snapshot. NVS writes a blob's new value before it lets go of the old one, so that room is what lets one snapshot replace another in one step, with the old one kept until the new one is whole. (If NVS ever says there is no room all the same, the old snapshot is erased and the write tried once more; that only costs the pages shown before the first poll.) The top of `storage.c` has the figures, and how this was tried against ESP-IDF's NVS code on a computer.

ESP-IDF's radio code also keeps its calibration data in the `nvs` partition, in a namespace of its own (`phy`). It holds nothing from a bundle, and a factory reset leaves it. The Wi-Fi driver's own storage is turned off (`CONFIG_ESP_WIFI_NVS_ENABLED`), so the network's name and password are never written anywhere but `dh_setup`.

When the enrolment secret has been used, the setup is rewritten without it. That too only marks the old copy as erased: the old copy, with the spent secret, stays in `nvs_setup` until NVS reclaims the page or a factory reset erases the partition.

### The storage key

The encryption keys are worked out at each start by the chip's HMAC unit from a secret key in eFuse key block **KEY5**. eFuses are one-time memory. [main/storage_key.c](main/storage_key.c) looks at the eFuses before NVS is started:

- **KEY5 holds an HMAC_UP key that is read-protected, write-protected and has its purpose locked**, which is how ESP-IDF's own key generation leaves it: encrypted NVS starts with it. Nothing is burnt.
- **KEY5 is empty** (purpose USER, all zeros, no protection), no other key block holds an HMAC_UP key, download mode and USB are allowed, Secure Boot and flash encryption are off, and no eFuse block reads back as damaged: this is **a new board**. Nothing is burnt at start-up and NVS isn't started. The board has nothing stored, so it reads as not set up, with no firmware keys, and answers `hello` like any other. **When its first setup arrives over USB and has passed every check**, just before the first write, the firmware reads the eFuses again, makes a random key with ESP-IDF's own function and **burns it into KEY5, permanently**, without asking. That uses KEY5 up for good, and is the intended behaviour: without it a new board could never be set up. (If the firmware declines to make the key before the call that burns is reached, say because the random number generator looks stuck, the bundle is refused with `storage_failed` and words that say to try again, and the board is still a new one: the next bundle tries again. If that call was reached and reported an error, the burn is never tried again in the same run, whatever the eFuses read: the display asks to be restarted, and the next start decides from what it finds.) The console says so in capitals, prints every key block and security setting before and after, and checks that nothing but KEY5's own bits changed. The firmware never sees the key again and never prints it.
- **Anything else** (KEY5 has another purpose or isn't empty, it holds an HMAC_UP key that isn't protected and locked, a block is damaged, any condition above fails): nothing is burnt and NVS isn't started. The screen shows **Storage problem** with the reasons, `hello` says `provisioned: false`, and a bundle is refused with `storage_failed`.

**Why the key isn't made at start-up.** ESP-IDF burns the key itself first and its purpose and protection second. A reset between the two leaves KEY5 holding a key with no purpose and no protection, which the firmware refuses for good. Just after an install is when a reset is most likely, because the app opening the USB port can reset the chip. When a `provision` arrives the port is open and the app is waiting for the answer, and the time at risk is a few milliseconds.

**A key that was never finished** (KEY5 not empty, purpose USER, unprotected) is not adopted, and no other key block is used in its place. It can't be told apart from a key someone loaded into a second-hand board and kept a copy of, and with that copy they could decrypt everything the display stores. It can't be finished either, since a key block can only be programmed once. So the screen says what happened ("KEY5 holds a key that was never finished: this display lost power or was reset while its storage key was being made, or someone else put a key there"), a bundle is refused with `storage_failed` and words of its own, and the board can't be set up with this firmware. Falling back to another empty block was looked at ([main/storage_key.h](main/storage_key.h) says why not).

ESP-IDF's `nvs_flash_init()` would burn a key into an empty KEY5 whatever state the chip is in, so **this firmware never calls it**, and nothing added later may. It starts NVS with `nvs_flash_read_security_cfg_v2()` and `nvs_flash_secure_init_partition()`, which can't burn anything. It never erases the `nvs` partition by itself either: if NVS won't start there, **Install firmware** (which erases the flash) is the way out. (`nvs_setup` is erased by a factory reset, and also at start-up if NVS says the partition itself can't be used as it is: no free pages, or a format from a newer NVS. The display then comes up waiting for setup. Any other failure to start or read storage, such as a flash read that failed or no memory, leaves the flash alone. The screen then shows **Storage problem** in amber and says to unplug the display and plug it back in, and to use **Install firmware** only if it keeps happening; a bundle is refused with `storage_failed` and the same advice. It says that nothing was erased only when that is true: if an erase of `nvs_setup` had begun (to finish a factory reset, or because NVS said the partition couldn't be used), it says the setup was erased, or that the erase didn't finish and the next start finishes it. (After a factory reset asked for while storage was out of use, whose mark can't be told to be in flash, it says instead that the setup may not have been erased, and to hold BOOT again once the display has restarted.) The same amber screen is shown if storage was working until a write failed and what it left couldn't be erased: nothing more is read or written until the display has restarted and read what is really there. The red **Storage problem** that points straight at **Install firmware** is kept for data that can't be used as it is: a flash layout without `nvs_setup`, or an `nvs` partition that NVS says must be erased.)

The scheme was checked on a real V2 board with an earlier build of this firmware: the burnt key reads back as all zeros (it can't be read out), what was stored isn't readable in the raw flash, and the board can still be erased and reflashed over USB, with the key outliving the erase.

What it protects, honestly: a plain read of the flash no longer shows the Wi-Fi password or the token. Someone with a cable who runs their own firmware on the chip can most likely still use its HMAC unit to decrypt.

## Factory reset

**Hold the BOOT button for 10 seconds while the display is running.** After the first second the screen counts down; letting go before the end cancels. At the end it says it is erasing, then how that went. (BOOT only puts the chip in its loader if it is held while the chip resets or is plugged in. Once the firmware is running it is an ordinary button.)

- **It erases** the `nvs_setup` partition: the hub addresses, port and trust anchors, the Wi-Fi details, the enrolment secret, the device ID and token, and the saved snapshot. NVS is stopped on it, every sector is erased and read back from flash as blank (0xFF), and NVS is started on it again, empty. Nothing of the setup is left in flash, not even as "erased" entries. It takes a second or two, and the console says `the setup partition was erased: all 131072 bytes at 0xc20000 read back as blank`.
- **A power cut during the erase** can't leave part of the setup behind. A mark is written in the `nvs` partition before the first sector is erased and taken out once NVS has started on the blank partition; a start that finds the mark finishes the erase before it reads anything. The mark is read back after it is written. Unless it reads as written, the reset doesn't begin: nothing is erased, the display carries on with its setup, and the screen says that the reset couldn't start, to hold BOOT for 10 seconds to try again, and to use **Install firmware** if it keeps happening. It goes on saying so until BOOT is held again or the setup changes: the status screen always shows it, and the line at the foot of each page shows it unless a hub problem or an unsaved token comes first. (A write that failed may have reached flash all the same, so the mark is then taken out again, and the reset counts as not started only once the mark reads as gone.) A mark that was already there, from a reset that didn't finish, is never taken out before the erase has finished. If the erase then fails, or the mark can't be taken out, the screen says **Reset not finished** and goes on saying it, a bundle is refused with `storage_failed` and words that say the same, and nothing is taken as a setup until it has been done: hold BOOT for 10 seconds again, or restart the display, which finishes the erase as it starts. The mark stays until then. That promise is made only with the mark known to be in flash. If the mark couldn't be written and then can't be read at all, nothing has been erased and the display can't tell whether its next start will erase the setup or go back to using it. The setup still goes out of use and no bundle is taken, but the screen says **Reset may have failed** instead: unplug the display, plug it back in and hold BOOT for 10 seconds again, and use **Install firmware** if it keeps happening. A bundle is refused with words that say the same.
- **A new board that has never been set up** has no storage key and nothing stored, so a reset erases nothing and touches no flash.
- **When storage isn't working.** If NVS started on the `nvs` partition (storage is out of use for this run after a read or a write that failed), the reset still erases `nvs_setup` in the same way, and the amber **Storage problem** screen then says the setup was erased. If NVS never started (no usable storage key, or an `nvs` partition that won't start), the mark can't be written, and a flash layout without `nvs_setup` has nothing to erase. Either way nothing is erased: the screen says so as soon as BOOT has been held for a second, with no countdown, and names **Install firmware**, which erases everything.
- **What it clears from memory.** The task that talks to the hub wipes its copy of the setup, the token and the pages, the screen forgets its pages, and the USB task drops a `provision` line it had only part-read or was still handling. A line that begins after the reset is answered as usual. It doesn't promise more than that: the USB driver's 20 KiB receive buffer keeps the bytes of the last lines it carried until later traffic overwrites them, and memory the HTTP client has freed isn't wiped. Reading either takes code running on the chip, and neither outlives unplugging the display.
- **It keeps** the `nvs` partition: the trusted firmware keys, the security floor and the update record. So a reset display can be paired with another hub, but still only accepts bundles naming the same firmware keys (or none): anything else is `firmware_keys_locked`, and only **Install firmware** changes the keys.
- The display goes straight back to **Waiting for setup**. It doesn't restart.
- **It never waits for the network.** The erase happens at once, whatever the display is doing. The task that talks to the hub works from its own copy of the setup; it finds the setup gone, gives up any request (within 50 ms once it is connected; a name lookup or a TCP connection under way runs out first, about ten seconds at most), leaves Wi-Fi and wipes the token and everything else it held. Every write it makes names the setup it belongs to, so nothing in flight can be stored after a reset, or beside a new setup.
- The hub isn't told. Remove the display in the desktop app as well, or it stays in the list.

The button is debounced (it must read the same for 100 ms), a button already down at start-up does nothing until it has been let go, and one hold does one reset. The `nvs` partition is never erased.

## The console

Open the board's serial port at any speed (opening it may restart the board). At start-up, and again every minute, the firmware prints read-only facts: the version and the slot it runs from, the board's revision and parts, memory, every eFuse key block and security setting, whether it is set up (with the count of hub addresses, whether Wi-Fi is set, whether a device token is held and the size of the saved snapshot, never their values), how many firmware keys it trusts, the security floor, the update attempts left, whether it is on trial and how the last update ended, and how much of the main, USB, hub and screen tasks' stacks has never been used. A download logs its progress every 256 KiB with the internal memory left and its lowest so far. The hub task logs each thing it does: joining Wi-Fi, enrolling (with the device ID the hub gave it, never the token), each poll with its page count and when the next is due, each heartbeat, and each failure with the status and error code.

## Tests without a board

[host-tests/](host-tests/) builds `dh_core` for a computer and runs it against every protocol vector and example, with gcc and clang and with the sanitisers:

```sh
cmake -S firmware/host-tests -B /tmp/dh-host && cmake --build /tmp/dh-host && ctest --test-dir /tmp/dh-host --output-on-failure
```

For signed updates, beside the vectors, the firmware's own part is tested too: `main/update.c` and `main/storage_fw.c` are built for a computer over a stand-in for the board ([host-tests/glue/](host-tests/glue/README.md)), every firmware vector is run again through them, and `update_glue_tests.c` covers what a vector can't describe (the two time limits, also against a hub that trickles bytes; flash calls and saves that fail, with a failed write that reached flash all the same; a partition that can't be set to boot; a factory reset mid-download; an image that was given up, which must be left unable to start; the trial boot's five minutes against a check-in, with nothing to go back to; and a start whose image state can't be read). For the screen it is `ui_tests.c` (ages and countdowns on the hub's clock, saved pages, shares and big numbers with every edge of their range, how each page is drawn, which page is kept when the pages change, and the burn-in shift). For talking to the hub it is `hub_tests.c` (the enrolment request and the heartbeat, the enrolment response and every error group, the stored device ID and token, which address and anchor to try next, when a heartbeat is due and when a snapshot is saved, the firmware report and the download's path). For setup over USB it is `usb_tests.c` (every bundle rule, every refusal in order, failing writes, when the store is told a write is coming (which is when a new board's storage key is made), the reply lines against `protocol/v1/examples/usb`, the stored form) and the `provision-*` vectors run through the whole handler. [host-tests/usb-e2e/](host-tests/usb-e2e/README.md) goes further on Linux: the desktop app's real sender against the firmware's handler over a pseudo-terminal. [fuzz/](fuzz/README.md) has the fuzz targets. [host-tests/idf-linux/](host-tests/idf-linux/README.md) has two harnesses that need ESP-IDF itself and so aren't part of `ctest`: ESP-IDF's NVS over this firmware's partition table (the room in the setup partition, and the factory reset's erase), and `main/hub_http.c` against a stand-in hub that trickles, floods and goes quiet at every stage of a request.

## Building

You need **ESP-IDF v5.5.6** exactly, installed with Espressif's [instructions](https://docs.espressif.com/projects/esp-idf/en/v5.5.6/esp32s3/get-started/index.html) (for ESP32-S3). Other versions are refused by `main/idf_component.yml`. Then, from the repository's root folder:

```sh
. "$IDF_PATH/export.sh"
idf.py -C firmware -B firmware/build build
```

A build folder keeps the settings it was first made with. After changing `sdkconfig.defaults`, delete the build folder (or its `sdkconfig`) and build again.

The first build downloads the components listed in [THIRD_PARTY.md](THIRD_PARTY.md) into `firmware/managed_components/`. The outputs land in the build folder: `display-hub.bin` (the app), `bootloader/bootloader.bin`, `partition_table/partition-table.bin`, `ota_data_initial.bin`, and `flasher_args.json`, which says where each goes.

Or build with Espressif's Docker image, as the GitHub checks do: `espressif/idf:v5.5.6`.

What makes a build the *official* firmware, and how a release's package is signed and checked, is in [official/README.md](official/README.md).

The settings are in [sdkconfig.defaults](sdkconfig.defaults), with a note on each. The build makes its full `sdkconfig` in the build folder, so change settings in `sdkconfig.defaults`, not with `menuconfig`. Builds are reproducible: the same source and ESP-IDF give the same image.

### Test builds

Two switches, for trying updates on a board. Both are off unless given on the command line, both are kept in the build folder (so give a test build a folder of its own), and neither is ever used for a release.

```sh
idf.py -C firmware -B build-update -DDH_TEST_VERSION=0.1.0-dev.8 build
idf.py -C firmware -B build-rollback -DDH_TEST_VERSION=0.1.0-dev.9 -DDH_TEST_TRIAL=no-check-in build
```

- `DH_TEST_VERSION` builds the same code as another version, so one build can be offered as an update to another. It must be a pre-release version (`0.1.0-dev.8`), which the version rules count as older than the plain `0.1.0`.
- `DH_TEST_TRIAL=no-check-in` makes firmware whose trial boot ignores every answer from the hub, so it must be sent back after five minutes. `DH_TEST_TRIAL=crash` makes firmware that also ignores the hub, and crashes 20 seconds into its trial boot, so the bootloader must send it back. Either needs `DH_TEST_VERSION` as well: test code can't be built with a release version. Installed over USB (not on trial) such a build behaves like any other.

The test code is two `#ifdef` blocks in `main/update.c`, and every line it logs starts `dh-test-trial`. To check an image has none of it: `grep -c dh-test-trial display-hub.bin` must print 0.

## Packing a development build

[tools/package-dev.sh](tools/package-dev.sh) signs a build with a key from `hub-firmware keygen` and makes a package the desktop app can install over USB:

```sh
firmware/tools/package-dev.sh firmware/build my-dev.dhkey display-hub-dev.dhfw
```

It reads the offsets from the build's `flasher_args.json` and the version from the built app, and asks for the key's passphrase (or set `DH_PASSPHRASE_FILE`). The key becomes the package's whole key set, so only use it for boards you're testing on. Never commit a key file.

## The flash layout

From [partitions.csv](partitions.csv). A first install sets it for good: an over-the-air update writes only the spare app slot.

**Boards installed before this layout must be reinstalled over USB once.** `nvs_setup` was added before the first release, so only development boards are affected. An over-the-air update can't add a partition: on a board with the older table, firmware that expects `nvs_setup` finds none, shows **Storage problem**, can't reach the hub on its trial boot, and is rolled back after five minutes, which uses up one update attempt each time it is offered. Nothing stops such an update being offered; **Install firmware** over USB is the fix.

| Part | Offset | Size |
|---|---|---|
| Bootloader | 0x0 | |
| Partition table | 0x8000 | |
| NVS, `nvs` (what a factory reset keeps) | 0x9000 | 64 KiB |
| OTA data (which slot to boot) | 0x19000 | 8 KiB |
| PHY calibration | 0x1b000 | 4 KiB |
| App slot `ota_0` (a first install writes here) | 0x20000 | 6 MiB |
| App slot `ota_1` | 0x620000 | 6 MiB |
| NVS for the setup, `nvs_setup` (what a factory reset erases) | 0xc20000 | 128 KiB |
| Free | 0xc40000 | about 3.75 MiB |

There is no factory slot. Secure Boot and flash encryption are off. NVS encryption is on for both NVS partitions, with the HMAC scheme and eFuse key block KEY5 (see [The storage key](#the-storage-key)). `nvs_setup` comes after the app slots so that nothing before it had to move.
