/*
 * What a display is given over USB the first time (docs/protocol-v1.md, "Provisioning bundle"):
 * where the hub is, the certificates to trust, a one-time enrolment secret and the Wi-Fi details.
 *
 * dh_bundle_read() checks a bundle with the rules of Bundle::problems() in
 * crates/hub-protocol/src/provisioning.rs and copies it out as a dh_setup. dh_setup_encode() and
 * dh_setup_decode() are its stored form, one blob, for the firmware to keep in encrypted NVS.
 *
 * A dh_setup holds secrets (the Wi-Fi password and the enrolment secret). Never log or show one,
 * and wipe a copy that is no longer needed with dh_setup_wipe(). It is about 9.5 KB: keep it in
 * static memory or PSRAM, not on a task's stack.
 */
#ifndef DH_SETUP_H
#define DH_SETUP_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "dh_crypto.h"
#include "dh_json.h"
#include "dh_update.h"

#ifdef __cplusplus
extern "C" {
#endif

#define DH_BUNDLE_FORMAT "display-hub.provisioning"
#define DH_BUNDLE_VERSION 1u
/* The fixed name in every hub certificate, whatever address the display connects to. */
#define DH_HUB_SERVER_NAME "hub.display-hub.internal"
/* The largest bundle, counted as the reference writes it out again (compact JSON). */
#define DH_MAX_BUNDLE_BYTES 12288u

#define DH_SETUP_MAX_HOSTS 4u
#define DH_SETUP_MAX_HOST_BYTES 253u
#define DH_SETUP_MAX_ANCHORS 2u
#define DH_SETUP_MAX_PEM_BYTES 4096u
#define DH_SETUP_MAX_SECRET_BYTES 64u
#define DH_SETUP_MAX_SSID_BYTES 32u
#define DH_SETUP_MIN_PASSWORD_BYTES 8u
#define DH_SETUP_MAX_PASSWORD_BYTES 63u
#define DH_SETUP_TIMESTAMP_BYTES 20u

/* Every text below is NUL-terminated for convenience, but only a host is sure to hold no NUL of
 * its own: use the lengths. */
typedef struct {
    uint8_t len; /* 1 to DH_SETUP_MAX_HOST_BYTES */
    char text[DH_SETUP_MAX_HOST_BYTES + 1u];
} dh_setup_host;

typedef struct {
    uint16_t pem_len; /* 27 to DH_SETUP_MAX_PEM_BYTES; starts "-----BEGIN CERTIFICATE-----" */
    char pem[DH_SETUP_MAX_PEM_BYTES + 1u];
    uint8_t spki_sha256[DH_SHA256_LEN];
} dh_setup_anchor;

typedef struct {
    uint8_t host_count; /* 1 to DH_SETUP_MAX_HOSTS, in the order to try them */
    dh_setup_host hosts[DH_SETUP_MAX_HOSTS];
    uint16_t port; /* never 0 */
    uint8_t anchor_count; /* 1 to DH_SETUP_MAX_ANCHORS */
    dh_setup_anchor anchors[DH_SETUP_MAX_ANCHORS];
    /* The one-time enrolment secret. From a bundle it is 1 to 64 bytes. A firmware clears it once
     * it has enrolled (length 0), and the stored form allows that. */
    uint8_t secret_len;
    char secret[DH_SETUP_MAX_SECRET_BYTES + 1u];
    char expires_at[DH_SETUP_TIMESTAMP_BYTES + 1u]; /* YYYY-MM-DDTHH:MM:SSZ */
    /* Wi-Fi. A bundle may come without it ("wifi": null). */
    bool has_wifi;
    uint8_t ssid_len; /* 1 to DH_SETUP_MAX_SSID_BYTES */
    char ssid[DH_SETUP_MAX_SSID_BYTES + 1u];
    uint8_t password_len; /* 0 (an open network), or 8 to DH_SETUP_MAX_PASSWORD_BYTES */
    char password[DH_SETUP_MAX_PASSWORD_BYTES + 1u];
} dh_setup;

typedef enum {
    /* Every rule passed: *out holds the bundle. */
    DH_BUNDLE_OK,
    /* Not the shape of a bundle at all (a member missing, repeated or of the wrong type, a number
     * that doesn't fit, a date that isn't one). The reference can't read such a line as a message
     * and ignores it, so there is no answer to give. */
    DH_BUNDLE_UNREADABLE,
    /* It breaks a rule: answer `invalid_bundle`. *problem says which, in fixed words that never
     * hold anything from the bundle. */
    DH_BUNDLE_INVALID,
} dh_bundle_result;

/*
 * Reads the `bundle` value of a parsed document. On DH_BUNDLE_OK, *out is the setup, *has_firmware
 * says whether the bundle had a `firmware` member and *firmware is that member (whether its keys
 * are valid points is dh_fw_bundle_check()'s question, not this function's). Otherwise *out is
 * wiped.
 */
dh_bundle_result dh_bundle_read(const dh_json_doc *doc, dh_json_value bundle, dh_setup *out,
                                bool *has_firmware, dh_fw_bundle *firmware,
                                const char **problem);

/* The stored form: a format number, then each field with its length. At most DH_SETUP_BLOB_MAX
 * bytes; dh_setup_encode() returns how many it wrote, or 0 for a setup that breaks a rule (it
 * never writes one that dh_setup_decode() would refuse). dh_setup_decode() checks every field
 * with the bundle's own rules and answers false, with *out wiped, for anything else: another
 * format, a length that doesn't add up, or a value out of range. */
#define DH_SETUP_BLOB_MAX 9464u
size_t dh_setup_encode(const dh_setup *setup, uint8_t out[DH_SETUP_BLOB_MAX]);
bool dh_setup_decode(dh_setup *out, const uint8_t *blob, size_t len);

/* Whether a setup keeps every rule a stored one must. */
bool dh_setup_valid(const dh_setup *setup);

/* Overwrites a setup, or any buffer that held a secret, with zeros in a way the compiler won't
 * remove. */
void dh_setup_wipe(dh_setup *setup);
void dh_wipe(void *p, size_t n);

#ifdef __cplusplus
}
#endif

#endif
