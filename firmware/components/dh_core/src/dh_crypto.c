#include "dh_crypto.h"

#include <string.h>

#include "mbedtls/bignum.h"
#include "mbedtls/ecdsa.h"
#include "mbedtls/ecp.h"

void dh_sha256_begin(dh_sha256 *h)
{
    memset(h, 0, sizeof *h);
    mbedtls_sha256_init(&h->ctx);
    h->live = true;
    h->failed = mbedtls_sha256_starts(&h->ctx, 0) != 0;
}

void dh_sha256_update(dh_sha256 *h, const uint8_t *data, size_t len)
{
    if (!h->live || h->failed || len == 0) {
        return;
    }
    if (mbedtls_sha256_update(&h->ctx, data, len) != 0) {
        h->failed = true;
    }
}

bool dh_sha256_finish(dh_sha256 *h, uint8_t out[DH_SHA256_LEN])
{
    bool ok = h->live && !h->failed && mbedtls_sha256_finish(&h->ctx, out) == 0;
    dh_sha256_abandon(h);
    if (!ok) {
        memset(out, 0, DH_SHA256_LEN);
    }
    return ok;
}

void dh_sha256_abandon(dh_sha256 *h)
{
    if (h->live) {
        mbedtls_sha256_free(&h->ctx);
    }
    h->live = false;
    h->failed = true;
}

bool dh_sha256_of(const uint8_t *data, size_t len, uint8_t out[DH_SHA256_LEN])
{
    dh_sha256 h;
    dh_sha256_begin(&h);
    dh_sha256_update(&h, data, len);
    return dh_sha256_finish(&h, out);
}

/* Reads key as a point on P-256 and checks it: both coordinates below the field prime, and on
 * the curve. An uncompressed encoding can't be the point at infinity, and P-256 has a cofactor
 * of one, so that is every check a public key needs. grp and q must be initialised, and are the
 * caller's to free. */
static bool load_key(mbedtls_ecp_group *grp, mbedtls_ecp_point *q,
                     const uint8_t key[DH_P256_KEY_LEN])
{
    return key[0] == 0x04 && mbedtls_ecp_group_load(grp, MBEDTLS_ECP_DP_SECP256R1) == 0 &&
           mbedtls_ecp_point_read_binary(grp, q, key, DH_P256_KEY_LEN) == 0 &&
           mbedtls_ecp_check_pubkey(grp, q) == 0;
}

bool dh_p256_key_valid(const uint8_t key[DH_P256_KEY_LEN])
{
    mbedtls_ecp_group grp;
    mbedtls_ecp_point q;
    mbedtls_ecp_group_init(&grp);
    mbedtls_ecp_point_init(&q);
    bool ok = load_key(&grp, &q, key);
    mbedtls_ecp_point_free(&q);
    mbedtls_ecp_group_free(&grp);
    return ok;
}

bool dh_p256_verify(const uint8_t key[DH_P256_KEY_LEN], const uint8_t digest[DH_SHA256_LEN],
                    const uint8_t signature[DH_P256_SIGNATURE_LEN])
{
    mbedtls_ecp_group grp;
    mbedtls_ecp_point q;
    mbedtls_mpi r, s;
    mbedtls_ecp_group_init(&grp);
    mbedtls_ecp_point_init(&q);
    mbedtls_mpi_init(&r);
    mbedtls_mpi_init(&s);
    /* mbedtls_ecdsa_verify() itself refuses r or s outside 1..n-1. */
    bool ok = load_key(&grp, &q, key) && mbedtls_mpi_read_binary(&r, signature, 32) == 0 &&
              mbedtls_mpi_read_binary(&s, signature + 32, 32) == 0 &&
              mbedtls_ecdsa_verify(&grp, digest, DH_SHA256_LEN, &q, &r, &s) == 0;
    mbedtls_mpi_free(&s);
    mbedtls_mpi_free(&r);
    mbedtls_ecp_point_free(&q);
    mbedtls_ecp_group_free(&grp);
    return ok;
}
