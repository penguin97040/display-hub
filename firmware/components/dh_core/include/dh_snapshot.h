/*
 * What a display does with one answer to GET /v1/device/snapshot (docs/protocol-v1.md, "What a
 * display does"; docs/firmware-handoff.md, "Every poll").
 *
 * dh_read_response() takes the HTTP status and body and says what to do: show a new snapshot,
 * keep the last good one and say why, and when to poll next. A snapshot comes back as plain
 * structs with every text already decoded and checked, ready for a screen layer to draw. Ages,
 * staleness and reset countdowns are worked out on the hub's clock with the dh_page_* functions,
 * given how long the snapshot has been held by a monotonic timer.
 *
 * It follows the reference display (tools/display-simulator, src/conform.rs), and passes every
 * file in protocol/v1/vectors.
 */
#ifndef DH_SNAPSHOT_H
#define DH_SNAPSHOT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "dh_limits.h"
#include "dh_text.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Room for so many characters of UTF-8, and a NUL. */
#define DH_UTF8_ROOM(chars) ((chars) * 4u + 1u)

typedef enum {
    DH_OUTCOME_SHOW,         /* draw the new snapshot */
    DH_OUTCOME_UNREADABLE,   /* keep the last good snapshot: the hub sent something unreadable */
    DH_OUTCOME_REPROVISION,  /* keep it: the display needs setting up again */
    DH_OUTCOME_INCOMPATIBLE, /* keep it: hub and display versions don't match */
    DH_OUTCOME_RETRY,        /* keep it: the hub is busy */
    DH_OUTCOME_FAULT,        /* keep it: the hub refused the request; log the code */
} dh_outcome;

typedef enum {
    DH_NEXT_REFRESH, /* refresh_after_seconds, +-10% */
    DH_NEXT_BACKOFF, /* 15 s doubling to 15 min, +-20% */
    DH_NEXT_HOURLY,  /* an hour (+-20%, as the reference display does) */
} dh_next_poll;

typedef enum {
    DH_ERROR_NONE, /* no structured error body */
    DH_ERROR_UNAUTHORISED,
    DH_ERROR_FORBIDDEN,
    DH_ERROR_TOKEN_REVOKED,
    DH_ERROR_UNKNOWN_DEVICE,
    DH_ERROR_ENROLMENT_USED,
    DH_ERROR_ENROLMENT_EXPIRED,
    DH_ERROR_INCOMPATIBLE_VERSION,
    DH_ERROR_BAD_REQUEST,
    DH_ERROR_PAYLOAD_TOO_LARGE,
    DH_ERROR_NOT_FOUND,
    DH_ERROR_CONFLICT,
    DH_ERROR_RATE_LIMITED,
    DH_ERROR_UNAVAILABLE,
    DH_ERROR_UNRECOGNISED, /* a code from a newer hub: treated as transient */
} dh_error_code;

/* Why a 200 answer was unreadable, for the log. */
typedef enum {
    DH_UNREADABLE_NONE,
    DH_UNREADABLE_TOO_LARGE,    /* over 32 KiB, refused before parsing */
    DH_UNREADABLE_NOT_JSON,     /* see json_status */
    DH_UNREADABLE_MESSAGE_TYPE, /* not a dashboard.snapshot */
    DH_UNREADABLE_MALFORMED,    /* a field missing, repeated or of the wrong type */
    DH_UNREADABLE_INVALID,      /* the envelope or a page's common fields break the rules */
} dh_unreadable;

typedef struct {
    dh_outcome outcome;
    dh_next_poll next_poll;
    dh_error_code error_code; /* for an error answer with a structured body */
    dh_unreadable unreadable;
    int json_status; /* a dh_json_status, for DH_UNREADABLE_NOT_JSON */
} dh_response;

typedef enum {
    DH_QUALITY_DEMO,
    DH_QUALITY_PROVIDER_REPORTED,
    DH_QUALITY_ESTIMATED,
    DH_QUALITY_MANUAL,
    DH_QUALITY_UNAVAILABLE,
    DH_QUALITY_UNRECOGNISED, /* from a newer hub: shown as unavailable */
} dh_quality;

typedef enum {
    DH_CANT_SHOW_NONE,
    DH_CANT_SHOW_UNKNOWN_KIND,
    DH_CANT_SHOW_BAD_DATA,
    DH_CANT_SHOW_TOO_LARGE,
} dh_cant_show;

#define DH_CANT_SHOW_TEXT "This display can't show this page."
#define DH_CANT_SHOW_UPDATE_HINT "It may need a firmware update."

typedef enum {
    DH_SHAPE_NONE, /* no figures: an unavailable page, or a card */
    DH_SHAPE_METER,
    DH_SHAPE_BIG_NUMBER,
    DH_SHAPE_TEXT,
    DH_SHAPE_LIST,
    DH_SHAPE_CUSTOM, /* a custom kind the caller said it supports */
} dh_shape;

typedef struct {
    char id[DH_MAX_ID_BYTES + 1];
    char label[DH_UTF8_ROOM(DH_MAX_LABEL_CHARS)];
    bool has_percent; /* false: unknown. Never draw it as 0. */
    double percent;
    bool has_resets_at;
    int64_t resets_at; /* seconds since 1970, on the hub's clock */
} dh_meter_item;

typedef struct {
    bool has_value; /* false: unknown. Draw a dash, never 0. */
    double value;
    uint8_t decimals;
    /* The figure to draw: `decimals` places, or "-" when unknown. */
    char text[32];
    bool has_unit;
    char unit[DH_UTF8_ROOM(DH_MAX_UNIT_CHARS)];
    bool has_caption;
    char caption[DH_UTF8_ROOM(DH_MAX_CAPTION_CHARS)];
} dh_big_number;

typedef struct {
    char text[DH_UTF8_ROOM(DH_MAX_LIST_TEXT_CHARS)];
    bool has_detail;
    char detail[DH_UTF8_ROOM(DH_MAX_LIST_TEXT_CHARS)];
} dh_list_item;

typedef struct {
    dh_shape shape;
    union {
        struct {
            size_t count;
            dh_meter_item items[DH_MAX_METER_ITEMS];
        } meter;
        dh_big_number big_number;
        struct {
            char text[DH_UTF8_ROOM(DH_MAX_TEXT_CHARS)];
        } text;
        struct {
            size_t count; /* may be 0: show "nothing listed" */
            dh_list_item items[DH_MAX_LIST_ITEMS];
        } list;
        struct {
            /* Where the page's data object is in the body that was read. Only valid while that
             * body is kept unchanged. */
            size_t data_offset;
            size_t data_length;
        } custom;
    } u;
} dh_content;

typedef struct {
    char id[DH_MAX_ID_BYTES + 1];
    char title[DH_UTF8_ROOM(DH_MAX_TITLE_CHARS)];
    /* The kind, when it is a shape name or a well-formed custom kind; empty otherwise. */
    char kind[DH_MAX_KIND_BYTES + 1];
    dh_quality quality;
    bool has_updated_at;
    int64_t updated_at;
    uint32_t stale_after_seconds;
    bool has_note;
    char note[DH_UTF8_ROOM(DH_MAX_NOTE_CHARS)];
    /* A "can't show this" card: draw the title and dh_card_lines(), nothing else. */
    dh_cant_show cant_show;
    /* What to draw when it isn't a card. DH_SHAPE_NONE for a page without figures: draw its
     * title and note. */
    dh_content content;
} dh_page;

typedef struct {
    bool present; /* the snapshot carried an offer in the right form */
    bool usable;  /* and it follows the rules: ids of lower-case hex, a size in range */
    char offer_id[DH_OFFER_ID_HEX_LEN + 1];
    char package_id[DH_PACKAGE_ID_HEX_LEN + 1];
    uint64_t size;
} dh_firmware_offer;

typedef struct {
    int64_t generated_at; /* the hub's clock when it built the snapshot */
    uint32_t refresh_after_seconds;
    size_t page_count; /* 0: "no pages yet" */
    dh_page pages[DH_MAX_PAGES];
    dh_firmware_offer firmware_update;
} dh_snapshot;

typedef struct {
    /* Whether this display draws a custom kind such as "nearby-flights/1". NULL: none, so every
     * custom kind is a card, as on the reference display. */
    bool (*custom_kind_supported)(const char *kind, void *ctx);
    void *ctx;
} dh_read_options;

/* Reads one answer. On DH_OUTCOME_SHOW, *out holds the new snapshot; otherwise *out is left in
 * an unspecified state, so read into a spare and keep the last good one. opts may be NULL. */
dh_response dh_read_response(int http_status, const uint8_t *body, size_t len,
                             const dh_read_options *opts, dh_snapshot *out,
                             dh_text_scratch *scratch);

/* The label every page shows for its figures: DEMO, REPORTED, ESTIMATE, MANUAL or UNAVAILABLE. */
const char *dh_quality_label(dh_quality q);
/* Whether figures of this quality are shown at all. */
bool dh_quality_shows_figures(dh_quality q);

/* The lines of a card under the title, and how many (1, or 2 for an unknown kind). */
size_t dh_card_lines(dh_cant_show reason, const char *lines[2]);
const char *dh_cant_show_name(dh_cant_show reason); /* "unknown_kind", "bad_data", "too_large" */

typedef struct {
    bool known; /* false: the page has never had figures */
    int64_t age_seconds;
    bool stale;
} dh_freshness;

/* Age on the hub's clock: (generated_at - updated_at) + held_seconds. */
dh_freshness dh_page_freshness(const dh_snapshot *s, const dh_page *p, int64_t held_seconds);

typedef struct {
    bool counting; /* the reset is ahead: show resets_in_seconds */
    int64_t resets_in_seconds;
    bool reset_due; /* it has passed: show "reset due" instead */
} dh_reset;

dh_reset dh_meter_item_reset(const dh_snapshot *s, const dh_meter_item *item,
                             int64_t held_seconds);

const char *dh_outcome_name(dh_outcome o);
const char *dh_next_poll_name(dh_next_poll n);
/* The code as the hub writes it ("unrecognised" for one from a newer hub), or NULL for none. */
const char *dh_error_code_name(dh_error_code c);

#ifdef __cplusplus
}
#endif

#endif
