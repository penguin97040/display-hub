/* The protocol's bounds (crates/hub-protocol/src/limits.rs). */
#ifndef DH_LIMITS_H
#define DH_LIMITS_H

#define DH_PROTOCOL_VERSION 1u
#define DH_MESSAGE_TYPE "dashboard.snapshot"

#define DH_MAX_SNAPSHOT_BYTES 32768u
#define DH_MAX_PAGES 10u
/* A page, counted as its compact serialised JSON. */
#define DH_MAX_PAGE_BYTES 3072u
#define DH_MAX_METER_ITEMS 6u
#define DH_MAX_LIST_ITEMS 8u
/* Characters (Unicode scalar values), not bytes. */
#define DH_MAX_TEXT_CHARS 280u
#define DH_MAX_LIST_TEXT_CHARS 48u
#define DH_MAX_UNIT_CHARS 8u
#define DH_MAX_CAPTION_CHARS 48u
#define DH_MAX_TITLE_CHARS 48u
#define DH_MAX_LABEL_CHARS 48u
#define DH_MAX_NOTE_CHARS 96u
/* Bytes: these are ASCII. */
#define DH_MAX_ID_BYTES 64u
#define DH_MAX_KIND_BYTES 64u

#define DH_MAX_BIG_NUMBER_ABS 1e15
#define DH_MAX_DECIMALS 3u
/* How deeply a custom page's data may nest; the data object itself is the first level. */
#define DH_MAX_CUSTOM_DEPTH 8u

#define DH_REFRESH_MIN_SECONDS 30u
#define DH_REFRESH_MAX_SECONDS 3600u
#define DH_STALE_MIN_SECONDS 60u
#define DH_STALE_MAX_SECONDS 604800u

/* Signed updates: the offer in a snapshot. */
#define DH_OFFER_ID_HEX_LEN 32u
#define DH_PACKAGE_ID_HEX_LEN 64u
#define DH_MAX_FIRMWARE_UPDATE_BYTES (137u + 4096u + 8u * 1024u * 1024u)

/* USB: the longest line, counting everything before the newline. */
#define DH_MAX_USB_LINE_BYTES 16384u

#endif
