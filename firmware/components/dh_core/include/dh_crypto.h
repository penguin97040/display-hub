/*
 * The two pieces of cryptography the signed-update core needs (docs/firmware-updates.md,
 * "Cryptography"): SHA-256, fed a piece at a time, and checking an ECDSA P-256 signature over a
 * SHA-256 digest, both from Mbed TLS 3.6.
 *
 * They use Mbed TLS's own SHA-256 and ECDSA calls rather than its PSA ones. Those keep nothing
 * between calls, so nothing here shares state with anything else: PSA has a key store that every
 * caller shares, and ESP-IDF builds Mbed TLS without the locks that would make that safe from two
 * tasks at once (CONFIG_MBEDTLS_THREADING_C is off by default). Everything that knows about Mbed
 * TLS is in dh_crypto.c, so a move to PSA later changes that one file.
 *
 * dh_core itself allocates nothing, but Mbed TLS takes a few kilobytes from the heap while it
 * checks a point or a signature, and gives them back. A function here that can't get what it
 * needs answers false, so a failure always reads as "not verified".
 */
#ifndef DH_CRYPTO_H
#define DH_CRYPTO_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "mbedtls/sha256.h"

#ifdef __cplusplus
extern "C" {
#endif

#define DH_SHA256_LEN 32u
/* An uncompressed SEC 1 point: 0x04, X, Y. */
#define DH_P256_KEY_LEN 65u
/* r then s, 32 bytes each, big-endian. Not DER. */
#define DH_P256_SIGNATURE_LEN 64u

/* A SHA-256 in progress: a small, fixed state however much is hashed. Every dh_sha256_begin()
 * must be matched by one dh_sha256_finish() or dh_sha256_abandon(). */
typedef struct {
    mbedtls_sha256_context ctx;
    bool live;   /* begun, and neither finished nor abandoned */
    bool failed; /* a step failed: finish will answer false */
} dh_sha256;

void dh_sha256_begin(dh_sha256 *h);
void dh_sha256_update(dh_sha256 *h, const uint8_t *data, size_t len);
/* Writes the digest and releases the state. False, with out zeroed, if any step failed. */
bool dh_sha256_finish(dh_sha256 *h, uint8_t out[DH_SHA256_LEN]);
/* Releases the state without a digest. Safe on a state that was never begun, if it is zeroed. */
void dh_sha256_abandon(dh_sha256 *h);

/* The SHA-256 of one buffer. */
bool dh_sha256_of(const uint8_t *data, size_t len, uint8_t out[DH_SHA256_LEN]);

/* Whether key is a valid P-256 public key: the 0x04 prefix, both coordinates in range, on the
 * curve, and not the point at infinity. */
bool dh_p256_key_valid(const uint8_t key[DH_P256_KEY_LEN]);

/* Whether signature is key's ECDSA signature over digest. False for a key that isn't a valid
 * point, and for r or s outside 1..n-1. */
bool dh_p256_verify(const uint8_t key[DH_P256_KEY_LEN], const uint8_t digest[DH_SHA256_LEN],
                    const uint8_t signature[DH_P256_SIGNATURE_LEN]);

#ifdef __cplusplus
}
#endif

#endif
