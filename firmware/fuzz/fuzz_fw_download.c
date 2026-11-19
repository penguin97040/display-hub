/*
 * Fuzzes one download through the streaming verifier, dh_fw_download_push(), in pieces of sizes
 * the input chooses.
 *
 * The input's first two bytes choose how it is used:
 *  - as a body, exactly as a hub might send it. A fuzzer can't forge a signature, so this reaches
 *    the header checks, the key comparison and the signature check, and little further;
 *  - or as a manifest and an image, which the target wraps in a header and signs with a key the
 *    display trusts (made when the target starts). That is what a hostile or careless maker
 *    could send, and it reaches the manifest reader, step 7 and the image.
 *
 * It checks what must hold whatever arrives: nothing is taken past the Content-Length, nothing
 * is written before the manifest has passed, a download only finishes if what was written is the
 * image the manifest names, for this display, and the same body gives the same answer however
 * it is cut into pieces.
 */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "dh_update.h"
#include "psa/crypto.h"

#define CHECK(c) \
    do { \
        if (!(c)) { \
            abort(); \
        } \
    } while (0)

#define DEVICE_TYPE "llm-usage-display"
#define HARDWARE "simulated"
#define SLOT_BYTES 65536u
#define MAX_BODY (DH_FW_HEADER_LEN + DH_FW_MAX_MANIFEST_LEN + SLOT_BYTES)

static psa_key_id_t signing_key;
static dh_fw_keys trusted;
static const dh_fw_device device = {DEVICE_TYPE, HARDWARE, SLOT_BYTES};

static dh_fw_download download;
static dh_text_scratch scratch;
static dh_fw_manifest reread;
static uint8_t body[MAX_BODY + 64];

static void make_key(void)
{
    static bool made;
    if (made) {
        return;
    }
    psa_key_attributes_t a = psa_key_attributes_init();
    psa_set_key_type(&a, PSA_KEY_TYPE_ECC_KEY_PAIR(PSA_ECC_FAMILY_SECP_R1));
    psa_set_key_bits(&a, 256);
    psa_set_key_usage_flags(&a, PSA_KEY_USAGE_SIGN_HASH);
    psa_set_key_algorithm(&a, PSA_ALG_ECDSA(PSA_ALG_SHA_256));
    size_t n = 0;
    dh_fw_keys_init(&trusted);
    CHECK(psa_crypto_init() == PSA_SUCCESS);
    CHECK(psa_generate_key(&a, &signing_key) == PSA_SUCCESS);
    CHECK(psa_export_public_key(signing_key, trusted.keys[1].bytes, DH_FW_KEY_LEN, &n) ==
          PSA_SUCCESS);
    CHECK(n == DH_FW_KEY_LEN);
    /* A second trusted key, which signs nothing: a body may name it and can never verify. */
    memset(trusted.keys[0].bytes, 0x04, DH_FW_KEY_LEN);
    trusted.key_count = 2;
    trusted.security_floor = 3;
    made = true;
}

/* Wraps a manifest and an image in a header signed by the trusted key. */
static size_t wrap(const uint8_t *manifest, size_t manifest_len, const uint8_t *image,
                   size_t image_len)
{
    static const uint8_t context[24] = "display-hub.firmware.v1";
    uint8_t digest[DH_SHA256_LEN];
    size_t n = 0;
    memset(body, 0, DH_FW_HEADER_LEN);
    memcpy(body, "DHFW", 4);
    body[4] = 1;
    body[5] = 1;
    body[6] = (uint8_t)(manifest_len & 0xffu);
    body[7] = (uint8_t)(manifest_len >> 8);
    memcpy(body + 8, trusted.keys[1].bytes, DH_FW_KEY_LEN);
    memcpy(body + DH_FW_HEADER_LEN, manifest, manifest_len);
    memcpy(body + DH_FW_HEADER_LEN + manifest_len, image, image_len);
    dh_sha256 h;
    dh_sha256_begin(&h);
    dh_sha256_update(&h, context, sizeof context);
    dh_sha256_update(&h, body, DH_FW_SIGNED_HEADER_LEN);
    dh_sha256_update(&h, manifest, manifest_len);
    CHECK(dh_sha256_finish(&h, digest));
    CHECK(psa_sign_hash(signing_key, PSA_ALG_ECDSA(PSA_ALG_SHA_256), digest, sizeof digest,
                        body + DH_FW_SIGNED_HEADER_LEN, DH_FW_SIGNATURE_LEN, &n) == PSA_SUCCESS);
    CHECK(n == DH_FW_SIGNATURE_LEN);
    return DH_FW_HEADER_LEN + manifest_len + image_len;
}

typedef struct {
    dh_fw_reason reason; /* NONE: finished */
    bool opened;
    uint64_t read;
    uint32_t written;
} outcome;

/* One download of body[0..len), in pieces whose sizes come from `seed` (0: all at once). */
static outcome run(size_t len, uint64_t content_length, uint8_t seed)
{
    outcome o = {DH_FW_REASON_NONE, false, 0, 0};
    dh_sha256 written_hash;
    bool finished = false, over = false;
    uint32_t x = seed;
    size_t at = 0;
    dh_sha256_begin(&written_hash);
    o.reason = dh_fw_download_begin(&download, &trusted, &device, content_length, true,
                                    content_length);
    over = o.reason != DH_FW_REASON_NONE;
    while (!over) {
        if (at == len) {
            o.reason = dh_fw_download_fail(&download, DH_FW_REASON_DOWNLOAD);
            break;
        }
        size_t piece = len - at;
        if (seed != 0) {
            x = x * 1664525u + 1013904223u;
            size_t want = 1u + ((x >> 16) % (seed < 128 ? 9u : 700u));
            piece = want < piece ? want : piece;
        }
        /* A copy of exactly the piece, so a read past its end is seen. */
        uint8_t *exact = malloc(piece);
        CHECK(exact != NULL);
        memcpy(exact, body + at, piece);
        size_t used = 0;
        while (used < piece && !over) {
            dh_fw_push p = dh_fw_download_push(&download, exact + used, piece - used);
            CHECK(p.used <= piece - used);
            CHECK(p.step != DH_FW_STEP_MORE || p.used == piece - used);
            if (p.write_len > 0) {
                CHECK(o.opened && p.step != DH_FW_STEP_ENDED && p.step != DH_FW_STEP_OPEN_SLOT);
                CHECK(p.write >= exact + used && p.write + p.write_len <= exact + used + p.used);
                dh_sha256_update(&written_hash, p.write, p.write_len);
                o.written += (uint32_t)p.write_len;
                CHECK(o.written <= SLOT_BYTES);
            }
            used += p.used;
            if (p.step == DH_FW_STEP_OPEN_SLOT) {
                CHECK(!o.opened);
                o.opened = true;
            } else if (p.step == DH_FW_STEP_FINISH) {
                finished = over = true;
            } else if (p.step == DH_FW_STEP_ENDED) {
                CHECK(p.reason != DH_FW_REASON_NONE);
                o.reason = p.reason;
                over = true;
            }
        }
        free(exact);
        at += used;
    }
    o.read = dh_fw_download_bytes_read(&download);
    CHECK(o.read == at && o.read <= content_length && o.read <= len);
    CHECK(o.opened == dh_fw_download_slot_open(&download));
    CHECK(finished == (o.reason == DH_FW_REASON_NONE));
    /* The spare partition is only ever opened for a manifest that is signed by the trusted key,
     * follows the rules, and is for this display. */
    const dh_fw_manifest *m = dh_fw_download_manifest(&download);
    CHECK((m != NULL) == o.opened);
    uint8_t digest[DH_SHA256_LEN];
    bool hashed = dh_sha256_finish(&written_hash, digest);
    if (o.opened) {
        uint16_t manifest_len = 0;
        CHECK(len >= DH_FW_HEADER_LEN && dh_fw_header_check(body, &manifest_len));
        CHECK(len >= (size_t)DH_FW_HEADER_LEN + manifest_len);
        CHECK(memcmp(body + 8, trusted.keys[1].bytes, DH_FW_KEY_LEN) == 0);
        CHECK(dh_fw_signature_valid(&trusted.keys[1], body, body + DH_FW_HEADER_LEN,
                                    manifest_len));
        CHECK(dh_fw_manifest_parse(body + DH_FW_HEADER_LEN, manifest_len, &reread, &scratch));
        CHECK(dh_fw_manifest_check(&reread, &trusted, &device, manifest_len, content_length) ==
              DH_FW_REASON_NONE);
        CHECK(strcmp(m->device_type, DEVICE_TYPE) == 0);
        CHECK(m->security_counter >= trusted.security_floor && m->image_size <= SLOT_BYTES);
        CHECK(o.written <= m->image_size);
        CHECK(o.reason == DH_FW_REASON_NONE || o.reason == DH_FW_REASON_DOWNLOAD ||
              o.reason == DH_FW_REASON_IMAGE_HASH);
    } else {
        CHECK(o.written == 0 && o.reason != DH_FW_REASON_NONE &&
              o.reason != DH_FW_REASON_IMAGE_HASH && o.reason != DH_FW_REASON_FLASH);
    }
    if (finished) {
        CHECK(hashed && o.written == m->image_size && o.read == content_length);
        CHECK(memcmp(digest, m->image_sha256, DH_SHA256_LEN) == 0);
    }
    /* Once over, nothing more is taken. */
    dh_fw_push again = dh_fw_download_push(&download, body, len);
    CHECK(again.used == 0 && again.write_len == 0 && again.step != DH_FW_STEP_MORE &&
          again.step != DH_FW_STEP_OPEN_SLOT);
    return o;
}

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size)
{
    make_key();
    if (size < 2) {
        return 0;
    }
    uint8_t mode = data[0], seed = data[1];
    data += 2;
    size -= 2;
    size_t len;
    if (mode & 1u) {
        /* A manifest, its length in the next two bytes, then an image. */
        if (size < 2) {
            return 0;
        }
        size_t manifest_len = (size_t)data[0] | ((size_t)data[1] << 8);
        data += 2;
        size -= 2;
        if (manifest_len > size) {
            manifest_len = size;
        }
        if (manifest_len < DH_FW_MIN_MANIFEST_LEN || manifest_len > DH_FW_MAX_MANIFEST_LEN) {
            return 0;
        }
        size_t image_len = size - manifest_len;
        if (image_len > SLOT_BYTES) {
            image_len = SLOT_BYTES;
        }
        len = wrap(data, manifest_len, data + manifest_len, image_len);
    } else {
        len = size < MAX_BODY ? size : MAX_BODY;
        if (len > 0) {
            memcpy(body, data, len);
        }
        if ((mode & 2u) && len >= DH_FW_HEADER_LEN) {
            /* Name a key the display trusts, so the signature is what is looked at. */
            memcpy(body + 8, trusted.keys[(mode >> 2) & 1u].bytes, DH_FW_KEY_LEN);
        }
    }
    /* The Content-Length: what is there, a little more, or a little less. */
    uint64_t content_length = len;
    switch ((mode >> 4) & 3u) {
    case 1:
        content_length = len + 1u + (mode >> 6);
        break;
    case 2:
        content_length = len > (mode >> 6) ? len - (mode >> 6) - 1u : 0;
        break;
    default:
        break;
    }
    outcome whole = run(len, content_length, 0);
    outcome pieces = run(len, content_length, seed ? seed : 1);
    CHECK(whole.reason == pieces.reason && whole.read == pieces.read &&
          whole.opened == pieces.opened);
    /* When the hash doesn't match, the last piece isn't handed over to be written. */
    CHECK(whole.written == pieces.written || whole.reason == DH_FW_REASON_IMAGE_HASH);
    return 0;
}
