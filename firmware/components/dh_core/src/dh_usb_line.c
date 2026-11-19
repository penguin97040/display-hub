#include "dh_usb_line.h"

void dh_usb_line_init(dh_usb_line_reader *r)
{
    r->len = 0;
    r->discarding = false;
}

dh_line_event dh_usb_line_feed(dh_usb_line_reader *r, const uint8_t *data, size_t len,
                               size_t *used, const uint8_t **line, size_t *line_len)
{
    for (size_t i = 0; i < len; i++) {
        uint8_t b = data[i];
        if (b == '\n') {
            if (r->discarding) {
                r->discarding = false;
                continue;
            }
            size_t n = r->len;
            r->len = 0;
            if (n > 0 && r->buf[n - 1] == '\r') {
                n--;
            }
            if (n > 0 && r->buf[0] == '{') {
                *used = i + 1;
                *line = r->buf;
                *line_len = n;
                return DH_LINE_JSON;
            }
            continue;
        }
        if (r->discarding) {
            continue;
        }
        r->buf[r->len++] = b;
        if (r->len >= DH_MAX_USB_LINE_BYTES) {
            r->len = 0;
            r->discarding = true;
            *used = i + 1;
            return DH_LINE_OVERLONG;
        }
    }
    *used = len;
    return DH_LINE_NONE;
}

void dh_usb_line_wipe(dh_usb_line_reader *r)
{
    if (r->len != 0) {
        return;
    }
    volatile uint8_t *b = r->buf;
    for (size_t i = 0; i < sizeof r->buf; i++) {
        b[i] = 0;
    }
}
