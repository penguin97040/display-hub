// Setup over USB. See usb_setup.h.
//
// LOG LINES AND REPLIES SHARE ONE PORT. The app ignores any line that doesn't start with "{", so
// log lines are harmless as long as one never lands in the middle of a reply. Two things make
// sure of that:
//
// - Both go through the same queue. Once the driver is installed and the console is moved onto
//   it (usb_setup_install), every log line and every reply is handed to the driver's one transmit
//   buffer. A reply is handed over whole, in a single usb_serial_jtag_write_bytes() call, which
//   the buffer takes as one piece.
// - A reply is only handed over while this task holds the console's own lock (stdout's). ESP_LOG
//   prints each log line with one vprintf() call, and vprintf() holds that lock for the whole
//   line, so while this task has it no log line is part-written, and none can start.
//
// As a last guard a reply starts with a newline, so it begins a line of its own whatever came
// before it (the app skips empty lines). What isn't covered: text the ROM prints straight to the
// port without the console (a crash report, or ESP_EARLY_LOG from an interrupt). `hello` is
// repeated by the app, so a spoilt one costs two seconds.
//
// WHAT STAYS IN MEMORY. A `provision` line holds the Wi-Fi password and the enrolment secret.
// This task wipes its own copies as soon as a line has been dealt with: the piece just read,
// the line reader's buffer and the handler's scratch. A factory reset also drops a line that
// was only part-read (usb_setup_forget). What can't be wiped is the driver's own receive
// buffer, RX_BUFFER_BYTES of ring buffer that ESP-IDF gives no call to reach: the bytes of the
// last lines to pass through it stay there until later traffic overwrites them (every `hello`
// does, a little at a time), or the display loses power. So a factory reset clears the setup
// from flash, every sector of it, and from this firmware's own buffers; it doesn't promise
// that no trace is left in RAM. Reading that takes code running on the chip, and RAM doesn't
// outlive a power cycle. (The buffer isn't emptied at a reset either: what is waiting in it
// unread was sent after the reset began, and is a line to answer.)
//
// What was read in ESP-IDF v5.5.6 to write this (paths are under components/):
// - esp_driver_usb_serial_jtag/src/usb_serial_jtag.c: the receive interrupt copies what arrives
//   into the driver's receive buffer and silently drops what doesn't fit (line 150), so that
//   buffer is made big enough for a whole 16 KiB line plus a `hello` even if this task is held
//   up. usb_serial_jtag_write_bytes() (265) puts its bytes in the transmit buffer in one
//   xRingbufferSend(), waiting for room up to the time given, and returns 0 if there wasn't any.
// - esp_driver_usb_serial_jtag/src/usb_serial_jtag_vfs.c: with the driver in use the console
//   writes a character at a time through the same call (695), and drops log text rather than
//   wait when the buffer stays full (nothing is reading the port), or when no computer is
//   attached at all (185). So logging never holds the firmware up.
// - newlib/src/reent_init.c: every task shares the same stdout, so its lock covers them all.
// - log/src/log_print.c: one vprintf() per log line.

#include "usb_setup.h"

#include <stdatomic.h>
#include <stdio.h>
#include <string.h>

#include "dh_setup.h"
#include "dh_usb.h"
#include "dh_usb_line.h"
#include "driver/usb_serial_jtag.h"
#include "driver/usb_serial_jtag_vfs.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "storage.h"

static const char *TAG = "usb-setup";

// The longest line the app sends is 16 KiB. Room for one, and the `hello`s around it.
#define RX_BUFFER_BYTES (DH_MAX_USB_LINE_BYTES + 4096)
// Must hold a whole reply at once (under 1 KiB), with room for log lines around it.
#define TX_BUFFER_BYTES 4096
_Static_assert(TX_BUFFER_BYTES >= 2 * (DH_USB_REPLY_MAX + 1), "a reply must fit the buffer");

// How long a reply waits for room in the transmit buffer. The app waits 10 seconds for an answer.
#define REPLY_WAIT_MS 1000

// JSON reading, a P-256 point check and encrypted NVS writes all run on this task. So does the
// making of a new board's storage key, with its eFuse reports and the first start of NVS
// (storage.c, store_prepare), which happens once in a board's life and can't be tried again:
// the stack has 4 KB more than the rest was seen to need.
#define TASK_STACK_BYTES 12288
#define TASK_PRIORITY    5

static const char *s_firmware_version;
static const char *s_hardware_revision;
static usb_setup_stored_t s_on_stored;
static TaskHandle_t s_task;
// How many factory resets there have been. Added to by usb_setup_forget() from another task,
// which holds storage's lock as it does (see there). This task notes the count as each piece
// is read from the port, and so for each line as it begins (usb_task), and reads it again
// with that lock held before the line is acted on: a line that began before a reset finished
// is dropped, and one that began after it is answered as usual.
static atomic_uint s_resets;

// Too big for a stack, and holding a bundle while one is read: taken from the heap once.
static dh_usb_line_reader *s_reader;
static dh_setup *s_work;
// A newline, then the reply line.
static char s_reply[1 + DH_USB_REPLY_MAX];

esp_err_t usb_setup_install(void)
{
    usb_serial_jtag_driver_config_t config = {
        .tx_buffer_size = TX_BUFFER_BYTES,
        .rx_buffer_size = RX_BUFFER_BYTES,
    };
    const esp_err_t err = usb_serial_jtag_driver_install(&config);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "the USB port's driver didn't start: %s", esp_err_to_name(err));
        return err;
    }
    // From here the console's output goes through the driver too, in one queue with the replies.
    usb_serial_jtag_vfs_use_driver();
    return ESP_OK;
}

// Hands one reply to the port, whole, with no log line inside it. See the top of this file.
static bool send_reply(size_t reply_len)
{
    s_reply[0] = '\n';
    flockfile(stdout);
    fflush(stdout);
    const int sent =
        usb_serial_jtag_write_bytes(s_reply, reply_len + 1, pdMS_TO_TICKS(REPLY_WAIT_MS));
    funlockfile(stdout);
    return sent == (int)(reply_len + 1);
}

// False if the line was dropped unread, because a factory reset came while it was arriving.
// `began` is the count of resets noted when the line began (usb_task).
static bool handle_line(const uint8_t *line, size_t len, unsigned began)
{
    // The keys, the record and the stored setup are shared with the rest of the firmware.
    storage_lock();
    // A factory reset adds to s_resets before it lets go of this lock (usb_setup_forget), and
    // a line waits here while the erase runs. So with the lock in hand and a count that has
    // moved on, this line was read, in whole or in part, before the reset finished: it is
    // dropped, never stored on the display that was just erased. A line that began after the
    // reset has the new count, and is answered.
    if (began != atomic_load(&s_resets)) {
        storage_unlock();
        dh_usb_line_wipe(s_reader);
        return false;
    }
    const dh_setup_store *store = storage_store();
    const storage_state_t state_before = storage_facts().state;
    // `hello` says nothing of the keys and the attempt budget once they may not be what flash
    // holds (storage_fw.h, "When a write fails"). The handler's own copies are left as they
    // are: a bundle is refused then, before it reads them.
    const dh_fw_keys *reported_keys = NULL;
    const dh_fw_record *reported_record = NULL;
    const bool reportable = storage_fw_reportable(&reported_keys, &reported_record);
    const dh_usb_display display = {
        .device_type = USB_SETUP_DEVICE_TYPE,
        .firmware_version = s_firmware_version,
        .hardware_revision = s_hardware_revision,
        .slot_bytes = storage_spare_slot_bytes(),
        .store = store,
        .keys = store != NULL ? storage_fw_keys() : NULL,
        .record = store != NULL ? storage_fw_record() : NULL,
        .work = s_work,
        .withhold_firmware = !reportable,
        // Asked, with the lock still held, only when a bundle is refused for want of storage.
        .storage_why = storage_usb_words,
    };
    const dh_usb_answer answer =
        dh_usb_handle_line(&display, line, len, s_reply + 1, DH_USB_REPLY_MAX);
    const storage_facts_t facts = storage_facts();
    // The hub task reads the stored setup for itself (hub.c), so no copy is kept here.
    dh_setup_wipe(s_work);
    storage_unlock();
    // The line may have been a bundle, with the Wi-Fi password in it.
    dh_usb_line_wipe(s_reader);

    bool sent = true;
    if (answer.reply_len > 0) {
        sent = send_reply(answer.reply_len);
    }

    // Only facts that are safe to print: never the line, the bundle or anything from it.
    switch (answer.outcome) {
    case DH_USB_HELLO:
        ESP_LOGI(TAG, "answered hello (set up: %s)", facts.set_up ? "yes" : "no");
        break;
    case DH_USB_PROVISIONED:
        ESP_LOGI(TAG, "bundle accepted and stored: %u hub %s, Wi-Fi %s, firmware keys %s",
                 (unsigned)facts.hub_hosts, facts.hub_hosts == 1 ? "address" : "addresses",
                 facts.wifi_set ? "set" : "not set",
                 answer.keys_taken ? "taken from the bundle" : "unchanged");
        if (s_on_stored != NULL) {
            s_on_stored();
        }
        break;
    case DH_USB_REFUSED:
        ESP_LOGW(TAG, "bundle refused: %s (%s)", dh_usb_error_name(answer.error), answer.why);
        // Getting storage ready for a new board's first setup can end with storage out of use
        // (storage.c, store_prepare). The screen is told, as it is of a stored setup.
        if (facts.state != state_before && s_on_stored != NULL) {
            s_on_stored();
        }
        break;
    default:
        ESP_LOGW(TAG, "ignored a line of %u bytes that isn't a message for a display",
                 (unsigned)len);
        break;
    }
    if (!sent) {
        ESP_LOGW(TAG, "the answer couldn't be sent: nothing is reading the port");
    }
    return true;
}

// After a factory reset: a line that is part-read, and began before the reset (`began` is the
// count noted when it did), is dropped, its bytes are wiped, and the rest of it is ignored as
// it comes. Looked at before and after each piece and whenever a read comes back empty, so
// within a second of the reset. Nothing else needs forgetting: the buffers are wiped after
// every line and every piece, and a whole line that began before the reset is dropped where
// it would be acted on (handle_line).
static void forget_if_reset(unsigned began)
{
    if (s_reader->len != 0 && began != atomic_load(&s_resets)) {
        dh_usb_line_forget(s_reader);
    }
}

static void usb_task(void *arg)
{
    (void)arg;
    static uint8_t chunk[256];
    // The count of resets when the line now being read began.
    unsigned line_resets = atomic_load(&s_resets);
    while (true) {
        // The count of resets is noted before the read, so a reset that finishes during the
        // read or straight after it always makes this piece's lines stale (see below).
        const unsigned piece_resets = atomic_load(&s_resets);
        // Keep reading whatever the display is doing, so the app's writes never stall.
        const int got = usb_serial_jtag_read_bytes(chunk, sizeof(chunk), pdMS_TO_TICKS(1000));
        if (got <= 0) {
            forget_if_reset(line_resets);
            continue;
        }
        // A factory reset can come at any moment: while this piece is being read or fed, or
        // while one of its lines waits for storage's lock. So the count of resets is noted
        // before the read, once for the whole piece, and every line that begins in this piece
        // is given that count, also one that begins after an earlier line of the piece has
        // waited out a reset. The count is looked at again with storage's lock held, for each
        // line (handle_line), so nothing read before a reset finished is acted on. A piece
        // whose read began after the reset has the new count, so the `hello` and the bundle
        // that follow a reset are answered at once. (A line that arrives while the reset is
        // under way may be dropped: the app sends `hello` again.)
        // A line part-read before the reset is dropped before any more of it is fed.
        forget_if_reset(line_resets);
        const uint8_t *piece = chunk;
        size_t left = (size_t)got;
        while (left > 0) {
            if (s_reader->len == 0) {
                // No line is part-read, so a line that comes out of this call began in this
                // piece. (With one part-read, a line that begins after it in the same call
                // is given the older count: dropped at worst, never the other way round.)
                line_resets = piece_resets;
            }
            size_t used = 0, len = 0;
            const uint8_t *line = NULL;
            const dh_line_event event =
                dh_usb_line_feed(s_reader, piece, left, &used, &line, &len);
            piece += used;
            left -= used;
            if (event == DH_LINE_JSON) {
                if (!handle_line(line, len, line_resets)) {
                    ESP_LOGW(TAG, "dropped a line of %u bytes unread: a factory reset came "
                                  "while it was arriving",
                             (unsigned)len);
                }
            } else if (event == DH_LINE_OVERLONG) {
                ESP_LOGW(TAG, "dropped a line longer than %u bytes", DH_MAX_USB_LINE_BYTES);
            }
        }
        dh_wipe(chunk, sizeof(chunk));
        forget_if_reset(line_resets);
        // A dropped line (too long, or not starting with '{') never reaches handle_line(), but
        // it may have been a bundle. This wipes the buffer only when no line is part-read.
        dh_usb_line_wipe(s_reader);
    }
}

esp_err_t usb_setup_start(const char *firmware_version, const char *hardware_revision,
                          usb_setup_stored_t on_stored)
{
    s_firmware_version = firmware_version;
    s_hardware_revision = hardware_revision;
    s_on_stored = on_stored;
    // From PSRAM if there is any: the internal memory is wanted for Wi-Fi and TLS.
    s_reader = heap_caps_calloc(1, sizeof(*s_reader), MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
    s_work = heap_caps_calloc(1, sizeof(*s_work), MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
    if (s_reader == NULL) {
        s_reader = heap_caps_calloc(1, sizeof(*s_reader), MALLOC_CAP_DEFAULT);
    }
    if (s_work == NULL) {
        s_work = heap_caps_calloc(1, sizeof(*s_work), MALLOC_CAP_DEFAULT);
    }
    if (s_reader == NULL || s_work == NULL) {
        ESP_LOGE(TAG, "out of memory for the USB task");
        return ESP_ERR_NO_MEM;
    }
    dh_usb_line_init(s_reader);
    if (xTaskCreate(usb_task, "usb-setup", TASK_STACK_BYTES, NULL, TASK_PRIORITY, &s_task) !=
        pdPASS) {
        ESP_LOGE(TAG, "the USB task didn't start");
        return ESP_ERR_NO_MEM;
    }
    return ESP_OK;
}

void usb_setup_forget(void)
{
    atomic_fetch_add(&s_resets, 1);
}

unsigned usb_setup_stack_unused(void)
{
    return s_task != NULL ? (unsigned)uxTaskGetStackHighWaterMark(s_task) : 0;
}
