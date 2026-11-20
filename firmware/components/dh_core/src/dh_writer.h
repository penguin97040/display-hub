/*
 * Writing small JSON messages into a caller's buffer: the USB replies (dh_usb.c) and the bodies
 * sent to the hub (dh_hub.c). Internal to dh_core.
 *
 * A writer never writes past its buffer and always keeps one byte for a NUL. Once something
 * didn't fit it is `full`, and whoever finishes the message must then give the caller nothing.
 */
#ifndef DH_WRITER_H
#define DH_WRITER_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "dh_limits.h"
#include "dh_update.h"

typedef struct {
    char *buf;
    size_t cap;
    size_t len;
    bool full;
} dh_writer;

static inline void dh_put_char(dh_writer *w, char c)
{
    /* One byte is always kept for the NUL. */
    if (w->full || w->len + 1u >= w->cap) {
        w->full = true;
        return;
    }
    w->buf[w->len++] = c;
}

static inline void dh_put(dh_writer *w, const char *s)
{
    while (*s != '\0') {
        dh_put_char(w, *s++);
    }
}

static inline void dh_put_escape(dh_writer *w, unsigned char c)
{
    static const char DIGITS[] = "0123456789abcdef";
    dh_put(w, "\\u00");
    dh_put_char(w, DIGITS[c >> 4]);
    dh_put_char(w, DIGITS[c & 15u]);
}

/* A JSON string. Anything but printable ASCII is written as an escape, so the message is plain
 * ASCII whatever it is given. */
static inline void dh_put_string(dh_writer *w, const char *s)
{
    dh_put_char(w, '"');
    for (; *s != '\0'; s++) {
        unsigned char c = (unsigned char)*s;
        if (c == '"' || c == '\\') {
            dh_put_char(w, '\\');
            dh_put_char(w, (char)c);
        } else if (c < 0x20 || c >= 0x7F) {
            /* The texts a display reports are ASCII, so a byte is a character here. */
            dh_put_escape(w, c);
        } else {
            dh_put_char(w, (char)c);
        }
    }
    dh_put_char(w, '"');
}

/* A JSON string from n bytes of UTF-8, which may hold any character, a NUL included. Bytes of
 * longer characters are passed on as they are, so the text must already be valid UTF-8. */
static inline void dh_put_utf8(dh_writer *w, const char *s, size_t n)
{
    dh_put_char(w, '"');
    for (size_t i = 0; i < n; i++) {
        unsigned char c = (unsigned char)s[i];
        if (c == '"' || c == '\\') {
            dh_put_char(w, '\\');
            dh_put_char(w, (char)c);
        } else if (c < 0x20 || c == 0x7F) {
            dh_put_escape(w, c);
        } else {
            dh_put_char(w, (char)c);
        }
    }
    dh_put_char(w, '"');
}

static inline void dh_put_number(dh_writer *w, uint32_t n)
{
    char digits[10];
    size_t count = 0;
    do {
        digits[count++] = (char)('0' + n % 10u);
        n /= 10u;
    } while (n > 0);
    while (count > 0) {
        dh_put_char(w, digits[--count]);
    }
}

/* The four fields every display reports about itself, as members with no braces around them. */
static inline void dh_put_device_fields(dh_writer *w, const char *device_type,
                                        const char *firmware_version,
                                        const char *hardware_revision)
{
    dh_put(w, "\"device_type\":");
    dh_put_string(w, device_type);
    dh_put(w, ",\"firmware_version\":");
    dh_put_string(w, firmware_version);
    dh_put(w, ",\"hardware_revision\":");
    dh_put_string(w, hardware_revision);
    dh_put(w, ",\"protocol_version\":");
    dh_put_number(w, DH_PROTOCOL_VERSION);
}

/* `,"firmware":{...}`: what a display that takes signed updates reports. Nothing is written,
 * and false comes back, when the keys or the record aren't known or a key's id can't be worked
 * out: better to leave the member out than to report a guess. */
static inline bool dh_put_firmware_member(dh_writer *w, const dh_fw_keys *keys,
                                          const dh_fw_record *record, uint32_t slot_bytes)
{
    char ids[DH_FW_MAX_KEYS][DH_PACKAGE_ID_HEX_LEN + 1];
    bool known = keys != NULL && record != NULL && keys->key_count <= DH_FW_MAX_KEYS &&
                 record->attempts_left <= DH_FW_FULL_BUDGET;
    for (uint8_t i = 0; known && i < keys->key_count; i++) {
        known = dh_fw_key_id_hex(&keys->keys[i], ids[i]);
    }
    if (!known) {
        return false;
    }
    dh_put(w, ",\"firmware\":{\"key_ids\":[");
    for (uint8_t i = 0; i < keys->key_count; i++) {
        if (i > 0) {
            dh_put_char(w, ',');
        }
        dh_put_string(w, ids[i]);
    }
    dh_put(w, "],\"security_floor\":");
    dh_put_number(w, keys->security_floor);
    dh_put(w, ",\"slot_bytes\":");
    dh_put_number(w, slot_bytes);
    dh_put(w, ",\"attempts_left\":");
    dh_put_number(w, record->attempts_left);
    dh_put_char(w, '}');
    return true;
}

#endif
