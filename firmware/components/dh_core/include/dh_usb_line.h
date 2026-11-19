/*
 * Splits the USB serial stream into lines (docs/protocol-v1.md, "Over USB"): one JSON object per
 * line ending in "\n", a "\r" before it dropped, lines that don't start with "{" ignored (boot logs
 * share the port), and a line of 16 KiB or more dropped whole. It never holds more than 16 KiB.
 * This is the reference's LineReader (crates/hub-protocol/src/usb.rs).
 */
#ifndef DH_USB_LINE_H
#define DH_USB_LINE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "dh_limits.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    uint8_t buf[DH_MAX_USB_LINE_BYTES];
    size_t len;
    bool discarding;
} dh_usb_line_reader;

typedef enum {
    DH_LINE_NONE,     /* all of the input was used, and no line is complete */
    DH_LINE_JSON,     /* a line starting with "{" */
    DH_LINE_OVERLONG, /* a line reached 16 KiB and is being dropped */
} dh_line_event;

void dh_usb_line_init(dh_usb_line_reader *r);

/* Takes bytes from data[0..len) until a line event, and sets *used to how many it took; call again
 * with the rest. For DH_LINE_JSON, *line and *line_len give the line without its ending; they stay
 * valid until the next call. */
dh_line_event dh_usb_line_feed(dh_usb_line_reader *r, const uint8_t *data, size_t len,
                               size_t *used, const uint8_t **line, size_t *line_len);

/* Overwrites whatever a finished line left in the buffer with zeros: a `provision` line holds the
 * Wi-Fi password and the enrolment secret. Call it once a DH_LINE_JSON line has been dealt with
 * and before the next feed; it does nothing while a line is part-read. */
void dh_usb_line_wipe(dh_usb_line_reader *r);

#ifdef __cplusplus
}
#endif

#endif
