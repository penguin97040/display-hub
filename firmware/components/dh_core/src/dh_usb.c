/*
 * The display's end of setup over USB. See dh_usb.h.
 *
 * A line is read as the reference reads it (ToDisplay in crates/hub-protocol/src/usb.rs, through
 * serde): an object whose `type` is "hello", with a `protocol_version` that is a plain integer
 * fitting 32 bits (any value: the reference answers them all), or "provision", with a `bundle`.
 * Unknown members are ignored, and a member this needs that is missing, repeated or of the wrong
 * type makes the whole line unreadable. An unreadable line gets no answer, whatever the display
 * holds: to the reference it is no different from a line of boot log.
 *
 * Where this differs from the reference:
 *  - a line holding bytes that aren't UTF-8 is ignored; the reference swaps them for U+FFFD and
 *    reads on;
 *  - an error's `message` is one fixed sentence, at most 200 bytes as the schema asks, never the
 *    reference's list of every problem or a storage error's own text;
 *  - the writes are made keys, record, setup (see dh_usb.h); the reference writes the bundle
 *    first, so a failure after it leaves the reference set up although it answered storage_failed;
 *  - a store that can't be read refuses a bundle with storage_failed before anything else.
 */
#include "dh_usb.h"

#include <string.h>

#include "dh_limits.h"
#include "dh_writer.h"

#define LIT(s) s, (sizeof s - 1u)

const char *dh_usb_error_name(dh_usb_error e)
{
    switch (e) {
    case DH_USB_ERROR_INVALID_BUNDLE:
        return "invalid_bundle";
    case DH_USB_ERROR_ALREADY_PROVISIONED:
        return "already_provisioned";
    case DH_USB_ERROR_STORAGE_FAILED:
        return "storage_failed";
    case DH_USB_ERROR_FIRMWARE_KEYS_LOCKED:
        return "firmware_keys_locked";
    default:
        return NULL;
    }
}

/* ---- Writing a reply ---- */

/* Ends the line. Returns its length, or 0 (and an empty buffer) if it didn't fit. */
static size_t finish(dh_writer *w)
{
    dh_put_char(w, '\n');
    if (w->full || w->cap == 0) {
        if (w->cap > 0) {
            w->buf[0] = '\0';
        }
        return 0;
    }
    w->buf[w->len] = '\0';
    return w->len;
}

static dh_held held(const dh_usb_display *d)
{
    return d->store == NULL ? DH_HELD_UNKNOWN : d->store->held(d->store->ctx);
}

size_t dh_usb_hello(const dh_usb_display *d, char *out, size_t cap)
{
    dh_writer w = {out, cap, 0, false};
    dh_put(&w, "{\"type\":\"hello\",");
    dh_put_device_fields(&w, d->device_type, d->firmware_version, d->hardware_revision);
    dh_put(&w, ",\"provisioned\":");
    dh_put(&w, held(d) == DH_HELD_SETUP ? "true" : "false");

    /* The firmware member, unless the keys and the record aren't known (storage isn't working),
     * or a key's id can't be worked out: better to leave it out than to report a guess. */
    if (d->store != NULL && !d->withhold_firmware) {
        (void)dh_put_firmware_member(&w, d->keys, d->record, d->slot_bytes);
    }
    dh_put_char(&w, '}');
    return finish(&w);
}

static dh_usb_answer refuse(dh_usb_error error, const char *why, char *reply, size_t cap)
{
    dh_writer w = {reply, cap, 0, false};
    dh_put(&w, "{\"type\":\"error\",\"code\":");
    dh_put_string(&w, dh_usb_error_name(error));
    dh_put(&w, ",\"message\":");
    dh_put_string(&w, why);
    dh_put_char(&w, '}');
    dh_usb_answer a = {DH_USB_REFUSED, error, why, finish(&w), false};
    return a;
}

/* ---- A bundle ---- */

/* The message for DH_HELD_RESTART_NEEDED. */
static const char RESTART_NEEDED[] =
    "This display couldn't save to its storage, so it can't be set up until it restarts. "
    "Unplug it, plug it back in, then try again.";

static dh_usb_answer provision(const dh_usb_display *d, const dh_json_doc *doc,
                               dh_json_value bundle, char *reply, size_t cap)
{
    const dh_usb_answer ignored = {DH_USB_IGNORED, DH_USB_ERROR_NONE, NULL, 0, false};
    dh_fw_bundle firmware;
    bool has_firmware = false;
    const char *problem = NULL;

    /* Read it first: a line the reference can't read is never answered, whatever is held. */
    dh_bundle_result read = dh_bundle_read(doc, bundle, d->work, &has_firmware, &firmware,
                                           &problem);
    if (read == DH_BUNDLE_UNREADABLE) {
        return ignored;
    }

    dh_held holds = held(d);
    if (holds == DH_HELD_UNKNOWN || d->keys == NULL || d->record == NULL) {
        dh_setup_wipe(d->work);
        return refuse(DH_USB_ERROR_STORAGE_FAILED,
                      "This display's storage isn't working, so it can't be set up.", reply, cap);
    }
    if (holds == DH_HELD_RESTART_NEEDED) {
        dh_setup_wipe(d->work);
        return refuse(DH_USB_ERROR_STORAGE_FAILED, RESTART_NEEDED, reply, cap);
    }
    if (holds == DH_HELD_SETUP) {
        dh_setup_wipe(d->work);
        return refuse(DH_USB_ERROR_ALREADY_PROVISIONED,
                      "Already set up; factory-reset this display first.", reply, cap);
    }
    if (read == DH_BUNDLE_INVALID) {
        return refuse(DH_USB_ERROR_INVALID_BUNDLE, problem, reply, cap);
    }

    /* The keys: every one a valid point before anything is stored, and the same set as the
     * display's own if it has any. Worked out on copies, so nothing changes unless it is saved. */
    dh_fw_keys keys = *d->keys;
    dh_fw_record record = *d->record;
    unsigned save = 0;
    dh_fw_bundle_answer fw =
        dh_fw_bundle_apply(&record, &keys, has_firmware ? &firmware : NULL, &save);
    if (fw != DH_FW_BUNDLE_OK) {
        dh_setup_wipe(d->work);
        return fw == DH_FW_BUNDLE_KEYS_LOCKED
                   ? refuse(DH_USB_ERROR_FIRMWARE_KEYS_LOCKED,
                            "This display trusts other firmware keys. Use Install firmware to "
                            "change them.",
                            reply, cap)
                   : refuse(DH_USB_ERROR_INVALID_BUNDLE,
                            "a key in firmware.trusted_keys isn't a valid P-256 public key", reply,
                            cap);
    }

    /* Keys, record, then the setup last: until that last write succeeds the display isn't set
     * up. What was saved is taken into memory straight away, so memory and flash always agree. */
    bool saved = true;
    if (saved && (save & DH_FW_SAVE_KEYS) != 0) {
        saved = d->store->save_keys(d->store->ctx, &keys);
        if (saved) {
            *d->keys = keys;
        }
    }
    if (saved && (save & DH_FW_SAVE_RECORD) != 0) {
        saved = d->store->save_record(d->store->ctx, &record);
        if (saved) {
            *d->record = record;
        }
    }
    saved = saved && d->store->save_setup(d->store->ctx, d->work);
    if (!saved) {
        dh_setup_wipe(d->work);
        /* If the write that failed has left the store needing a restart, sending the bundle
         * again can't work until then: say so now, not at the next try. */
        return refuse(DH_USB_ERROR_STORAGE_FAILED,
                      held(d) == DH_HELD_RESTART_NEEDED
                          ? RESTART_NEEDED
                          : "This display couldn't save its setup. Try again.",
                      reply, cap);
    }

    dh_writer w = {reply, cap, 0, false};
    dh_put(&w, "{\"type\":\"provisioned\"}");
    dh_usb_answer a = {DH_USB_PROVISIONED, DH_USB_ERROR_NONE, NULL, finish(&w),
                       (save & DH_FW_SAVE_KEYS) != 0};
    return a;
}

dh_usb_answer dh_usb_handle_line(const dh_usb_display *d, const uint8_t *line, size_t len,
                                 char *reply, size_t cap)
{
    const dh_usb_answer ignored = {DH_USB_IGNORED, DH_USB_ERROR_NONE, NULL, 0, false};
    if (cap > 0) {
        reply[0] = '\0';
    }
    if (cap < DH_USB_REPLY_MAX || len == 0 || line[0] != '{') {
        return ignored;
    }

    dh_json_doc doc;
    dh_json_value root = 0, type, v;
    dh_json_options opts = {DH_MAX_USB_LINE_BYTES, 0, 0};
    if (dh_json_parse(&doc, line, len, &opts, &root) != DH_JSON_OK ||
        dh_json_type_of(&doc, root) != DH_JSON_OBJECT ||
        dh_json_object_find(&doc, root, LIT("type"), &type) != 1 ||
        dh_json_type_of(&doc, type) != DH_JSON_STRING) {
        return ignored;
    }

    if (dh_json_string_equals(&doc, type, LIT("hello"))) {
        uint64_t version = 0;
        if (dh_json_object_find(&doc, root, LIT("protocol_version"), &v) != 1 ||
            dh_json_type_of(&doc, v) != DH_JSON_NUMBER ||
            !dh_json_number_u64(&doc, v, &version) || version > UINT32_MAX) {
            return ignored;
        }
        dh_usb_answer a = {DH_USB_HELLO, DH_USB_ERROR_NONE, NULL, dh_usb_hello(d, reply, cap),
                           false};
        return a;
    }
    if (dh_json_string_equals(&doc, type, LIT("provision"))) {
        if (dh_json_object_find(&doc, root, LIT("bundle"), &v) != 1) {
            return ignored;
        }
        return provision(d, &doc, v, reply, cap);
    }
    return ignored;
}

bool dh_usb_factory_reset(const dh_usb_display *d)
{
    return d->store != NULL && d->store->erase_setup(d->store->ctx);
}
