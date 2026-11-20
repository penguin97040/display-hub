// One HTTPS request to the hub, at one address, with one trust anchor
// (docs/firmware-handoff.md, "TLS, exactly").
//
// Every request is made the same way, whatever it is for:
// - the anchor's certificate is the only one trusted: no bundle of public authorities, no global
//   store;
// - the certificate must be for hub.display-hub.internal, whatever address was connected to;
// - TLS 1.2 or 1.3, as the hub chooses;
// - a redirect is never followed: it is an answer like any other, and reads as an error;
// - no more of the answer is read than the caller allows.
//
// Nothing here logs a header, a body or an address.

#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "dh_hub.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    const char *host; // one of the setup's hub addresses, NUL-terminated
    uint16_t port;
    const char *anchor_pem; // one certificate in PEM, NUL-terminated
    const char *path;
    // NULL for a GET; otherwise the JSON body of a POST, body_len bytes.
    const char *body;
    size_t body_len;
    // NULL, or "Bearer ...".
    const char *authorization;
    // Where the answer's body goes. At most ok_cap bytes of a 200 answer are kept, and at most
    // error_cap of any other; the buffer must hold one byte more than the larger of the two.
    uint8_t *answer;
    size_t ok_cap;
    size_t error_cap;
    // Asked between steps: true gives the request up (a factory reset has happened).
    bool (*stop)(void);
} hub_request_t;

typedef struct {
    // DH_TRY_ANSWERED only when the hub proved who it is and its whole answer arrived (or as much
    // of it as was allowed).
    dh_try_result result;
    int status;
    // The bytes kept. One more than the cap for that status means the body was longer than
    // allowed: it was not read to the end, and must be treated as too large.
    size_t len;
    bool stopped;
} hub_answer_t;

hub_answer_t hub_http_request(const hub_request_t *request);

// ---- A firmware download (docs/firmware-updates.md, "Checks before installing") ----
//
// The same connection as any other request: the same anchor, name check and TLS versions, and
// no redirect followed. What differs is that a 200 answer's body isn't read here: its length is
// handed back first, and the caller then reads it a piece at a time, so the update core sees
// every byte before anything is written.

typedef struct hub_download hub_download_t;

typedef struct {
    // As for hub_answer_t. With DH_TRY_ANSWERED and a status other than 200, `len` bytes of the
    // error answer are in the request's buffer (at most error_cap) and there is no download.
    dh_try_result result;
    int status;
    size_t len;
    bool stopped;
    // For a 200 answer: whether it came with a Content-Length (and not chunked), and its value.
    bool has_length;
    uint64_t length;
} hub_download_answer_t;

// Sends the request (a GET) and reads the answer's status and headers. Only for a 200 answer is
// *out set, to a download the caller must close; none of its body has been handed over yet.
hub_download_answer_t hub_http_download_open(const hub_request_t *request, hub_download_t **out);

// Reads up to `cap` bytes of the body, waiting at most a few seconds. Returns how many were
// read; 0 if none came in that time (ask again: the caller keeps the time limits); or a negative
// number if the connection broke or the body has ended.
int hub_http_download_read(hub_download_t *download, uint8_t *out, size_t cap);

void hub_http_download_close(hub_download_t *download);

#ifdef __cplusplus
}
#endif
