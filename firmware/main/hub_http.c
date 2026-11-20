// One HTTPS request to the hub. See hub_http.h.
//
// What was read in ESP-IDF v5.5.6 to write this (paths are under components/):
// - esp_http_client/esp_http_client.c: a redirect is only ever followed inside
//   esp_http_client_perform() (esp_http_check_response, line 1500). This file never calls it: it
//   opens, writes, fetches the headers and reads, so a 3xx status is simply handed back.
//   disable_auto_redirect is set as well.
// - esp-tls/esp_tls_mbedtls.c: the name checked, and sent as SNI, is common_name when it is set
//   (not the address); the anchor is cert_pem and nothing else unless a bundle or the global
//   store is asked for, and neither is. When the handshake fails it records Mbed TLS's error and,
//   if a certificate was presented, the verification flags (lines 320-330, 507-513).
// - esp-tls/esp_tls.c: a name is looked up with getaddrinfo() (line 242), which is lwIP's.
// - esp_http_client.c again, for a download: esp_http_client_fetch_headers() answers 0 and marks
//   the response chunked when there is no Content-Length above zero (line 1608), so "chunked" is
//   also how a missing length reads; esp_http_client_read() never asks the transport for more
//   than the caller wants or the receive buffer holds, stops at the Content-Length, and answers
//   -ESP_ERR_HTTP_EAGAIN when a wait runs out with nothing read (line 1403). But it reads the
//   connection again and again until it has all it was asked for.
// - esp-tls/esp_tls.c and mbedtls/port/net_sockets.c, for a download: the socket is a blocking
//   one whose every recv() waits up to the connect timeout (SO_RCVTIMEO, esp_tls.c line 306),
//   and Mbed TLS calls recv() until it has a whole TLS record, however slowly its bytes come.
//   So one read of a blocking connection can't be bounded from outside. With the socket made
//   non-blocking (esp_http_client_get_socket() hands it out), a record that isn't all there
//   yet reads as "nothing yet" (net_sockets.c line 354, transport_ssl.c line 277), Mbed TLS
//   keeps what it has, and the read comes straight back. hub_http_download_open() does that
//   once the headers are in.

#include "hub_http.h"

#include <fcntl.h>
#include <stdlib.h>
#include <string.h>

#include "dh_setup.h"
#include "esp_http_client.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "esp_tls.h"
#include "mbedtls/ssl.h"
#include "mbedtls/x509.h"

static const char *TAG = "hub-http";

// For connecting, and for each wait for more of the answer.
#define STEP_TIMEOUT_MS 10000
// For the whole answer, however slowly it comes.
#define ANSWER_TIMEOUT_MS 30000
// The hub's response headers are a few hundred bytes; the request's are under 400.
#define RX_BUFFER_BYTES 2048
#define TX_BUFFER_BYTES 1024
// How long one read of a download's connection waits for bytes before esp_http_client_read()
// hands back to the caller, which keeps the download's own time limits and looks for a factory
// reset in between. It is short because that call reads the connection again and again until it
// has all it was asked for, and gives up only when one of those reads waits this long for
// nothing: see update.c, "How long one read can take".
#define DOWNLOAD_STEP_TIMEOUT_MS 50
// ESP-IDF's own log tags for the connection. With a non-blocking socket they print an error
// each time part of a TLS record has arrived and the rest hasn't, which is many times a
// download and means nothing. They are turned off while a download is read; what goes wrong
// is logged here and in update.c.
#define TRANSPORT_LOG_TAG   "transport_base"
#define HTTP_CLIENT_LOG_TAG "HTTP_CLIENT"

static bool stopped(const hub_request_t *request)
{
    return request->stop != NULL && request->stop();
}

// Why the connection wasn't made: something answered with a certificate the anchor doesn't
// vouch for, or nothing useful answered at all.
static dh_try_result connect_failure(esp_http_client_handle_t client, esp_err_t open_err)
{
    int tls_code = 0, tls_flags = 0;
    const esp_err_t last = esp_http_client_get_and_clear_last_tls_error(client, &tls_code,
                                                                        &tls_flags);
    // esp-tls records Mbed TLS's error as a positive number. Mbed TLS ends a handshake with one
    // of these two when the certificate isn't vouched for by the anchor or isn't for the hub's
    // name (ssl_tls.c, mbedtls_ssl_verify_certificate). The flags are only there if esp-tls
    // could still see the certificate afterwards.
    const bool mismatch = tls_flags != 0 || tls_code == -MBEDTLS_ERR_X509_CERT_VERIFY_FAILED ||
                          tls_code == -MBEDTLS_ERR_SSL_BAD_CERTIFICATE;
    ESP_LOGW(TAG, "no connection: %s (%s; TLS error -0x%04x, certificate flags 0x%x)",
             mismatch ? "the certificate isn't the hub's" : "nothing answered as a hub",
             esp_err_to_name(last != ESP_OK ? last : open_err), (unsigned)tls_code,
             (unsigned)tls_flags);
    return mismatch ? DH_TRY_WRONG_HUB : DH_TRY_NO_ANSWER;
}

// Sets a request up and connects: looks the name up, does the TLS handshake and sends the
// request's headers. NULL, with *result saying why, if no connection was made.
static esp_http_client_handle_t open_request(const hub_request_t *request, dh_try_result *result)
{
    *result = DH_TRY_NO_ANSWER;
    const esp_http_client_config_t config = {
        .host = request->host,
        .port = request->port,
        .path = request->path,
        .transport_type = HTTP_TRANSPORT_OVER_SSL,
        .method = request->body != NULL ? HTTP_METHOD_POST : HTTP_METHOD_GET,
        // The hub's own certificate is the only trust anchor.
        .cert_pem = request->anchor_pem,
        .use_global_ca_store = false,
        .crt_bundle_attach = NULL,
        // The fixed name in every hub certificate, whatever address this connects to.
        .common_name = DH_HUB_SERVER_NAME,
        .skip_cert_common_name_check = false,
        // TLS 1.2 or 1.3.
        .tls_version = ESP_HTTP_CLIENT_TLS_VER_ANY,
        .disable_auto_redirect = true,
        .max_authorization_retries = -1,
        .timeout_ms = STEP_TIMEOUT_MS,
        .buffer_size = RX_BUFFER_BYTES,
        .buffer_size_tx = TX_BUFFER_BYTES,
        .keep_alive_enable = false,
    };
    esp_http_client_handle_t client = esp_http_client_init(&config);
    if (client == NULL) {
        ESP_LOGE(TAG, "couldn't set a request up (out of memory?)");
        return NULL;
    }

    esp_err_t err = esp_http_client_set_header(client, DH_PROTOCOL_HEADER,
                                               DH_PROTOCOL_HEADER_VALUE);
    if (err == ESP_OK && request->authorization != NULL) {
        err = esp_http_client_set_header(client, "Authorization", request->authorization);
    }
    if (err == ESP_OK && request->body != NULL) {
        err = esp_http_client_set_header(client, "Content-Type", "application/json");
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "couldn't set a request's headers: %s", esp_err_to_name(err));
        esp_http_client_cleanup(client);
        return NULL;
    }

    err = esp_http_client_open(client, request->body != NULL ? (int)request->body_len : 0);
    if (err != ESP_OK) {
        *result = connect_failure(client, err);
        esp_http_client_cleanup(client);
        return NULL;
    }
    // From here the other end has proved it is the hub.
    return client;
}

// Reads an answer's body into the request's buffer: to its end, or one byte past `cap`. True if
// the whole of it arrived, or as much as was allowed.
static bool read_answer(esp_http_client_handle_t client, const hub_request_t *request,
                        int64_t content_length, size_t cap, int64_t started_ms,
                        hub_answer_t *answer)
{
    if (!esp_http_client_is_chunked_response(client) && content_length > (int64_t)cap) {
        // Too long by its own account: don't read any of it.
        answer->len = cap + 1;
        return true;
    }
    // Read until the end, or one byte past what is allowed.
    while (answer->len <= cap) {
        if (stopped(request)) {
            answer->stopped = true;
            return false;
        }
        if (esp_timer_get_time() / 1000 - started_ms > ANSWER_TIMEOUT_MS) {
            ESP_LOGW(TAG, "the answer took too long");
            return false;
        }
        // A piece at a time, so the checks above are made often.
        size_t want = cap + 1 - answer->len;
        if (want > RX_BUFFER_BYTES) {
            want = RX_BUFFER_BYTES;
        }
        const int got =
            esp_http_client_read(client, (char *)request->answer + answer->len, (int)want);
        if (got < 0) {
            ESP_LOGW(TAG, "the answer broke off");
            return false;
        }
        if (got == 0) {
            // The end, or a wait that ran out with the body unfinished.
            if (!esp_http_client_is_complete_data_received(client)) {
                ESP_LOGW(TAG, "the answer stopped part-way");
                return false;
            }
            return true;
        }
        answer->len += (size_t)got;
    }
    return true;
}

hub_answer_t hub_http_request(const hub_request_t *request)
{
    hub_answer_t answer = {.result = DH_TRY_NO_ANSWER};
    if (stopped(request)) {
        answer.stopped = true;
        return answer;
    }

    const int64_t started_ms = esp_timer_get_time() / 1000;
    esp_http_client_handle_t client = open_request(request, &answer.result);
    if (client == NULL) {
        return answer;
    }

    bool whole = false;
    if (stopped(request)) {
        answer.stopped = true;
    } else if (request->body != NULL &&
               esp_http_client_write(client, request->body, (int)request->body_len) !=
                   (int)request->body_len) {
        ESP_LOGW(TAG, "the request couldn't be sent");
    } else {
        const int64_t content_length = esp_http_client_fetch_headers(client);
        const int status = esp_http_client_get_status_code(client);
        if (content_length < 0 || status < 100) {
            ESP_LOGW(TAG, "no answer to the request");
        } else {
            answer.status = status;
            const size_t cap = status == 200 ? request->ok_cap : request->error_cap;
            whole = read_answer(client, request, content_length, cap, started_ms, &answer);
        }
    }
    esp_http_client_close(client);
    esp_http_client_cleanup(client);

    if (whole && !answer.stopped) {
        answer.result = DH_TRY_ANSWERED;
    } else {
        answer.result = DH_TRY_NO_ANSWER;
        answer.status = 0;
        answer.len = 0;
    }
    return answer;
}

// ---- A firmware download ----

// A download is its connection, and the log levels to put back when it is closed.
struct hub_download {
    esp_http_client_handle_t client;
    esp_log_level_t transport_level;
    esp_log_level_t http_client_level;
};

// Makes the connection's socket non-blocking, so no read of it can be held open by a hub that
// sends a TLS record a byte at a time (see the top of this file).
static bool make_non_blocking(esp_http_client_handle_t client)
{
    const int fd = esp_http_client_get_socket(client);
    const int flags = fd >= 0 ? fcntl(fd, F_GETFL, 0) : -1;
    return flags >= 0 && fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0;
}

hub_download_answer_t hub_http_download_open(const hub_request_t *request, hub_download_t **out)
{
    hub_download_answer_t answer = {.result = DH_TRY_NO_ANSWER};
    *out = NULL;
    if (stopped(request)) {
        answer.stopped = true;
        return answer;
    }

    const int64_t started_ms = esp_timer_get_time() / 1000;
    esp_http_client_handle_t client = open_request(request, &answer.result);
    if (client == NULL) {
        return answer;
    }

    bool keep = false, whole = false;
    if (stopped(request)) {
        answer.stopped = true;
    } else {
        const int64_t content_length = esp_http_client_fetch_headers(client);
        const int status = esp_http_client_get_status_code(client);
        if (content_length < 0 || status < 100) {
            ESP_LOGW(TAG, "no answer to the request");
        } else if (status == 200) {
            // The body is the caller's to read. A response without a length of its own, chunked
            // or not, is one the update core refuses before any of it is read.
            answer.status = status;
            answer.has_length = content_length > 0 && !esp_http_client_is_chunked_response(client);
            answer.length = answer.has_length ? (uint64_t)content_length : 0;
            // Short waits and a non-blocking socket from here, so no read can be held open and
            // the caller's own time limits are looked at often.
            keep = whole = make_non_blocking(client) &&
                           esp_http_client_set_timeout_ms(client, DOWNLOAD_STEP_TIMEOUT_MS) ==
                               ESP_OK;
            if (!keep) {
                ESP_LOGE(TAG, "couldn't ready the connection for a download");
            }
        } else {
            // An error answer, read like any other.
            hub_answer_t error = {.result = DH_TRY_NO_ANSWER};
            answer.status = status;
            whole = read_answer(client, request, content_length, request->error_cap, started_ms,
                                &error);
            answer.len = error.len;
            answer.stopped = error.stopped;
        }
    }

    if (keep) {
        hub_download_t *download = calloc(1, sizeof(*download));
        if (download != NULL) {
            download->client = client;
            download->transport_level = esp_log_level_get(TRANSPORT_LOG_TAG);
            download->http_client_level = esp_log_level_get(HTTP_CLIENT_LOG_TAG);
            esp_log_level_set(TRANSPORT_LOG_TAG, ESP_LOG_NONE);
            esp_log_level_set(HTTP_CLIENT_LOG_TAG, ESP_LOG_NONE);
            *out = download;
            answer.result = DH_TRY_ANSWERED;
            return answer;
        }
        ESP_LOGE(TAG, "out of memory for a download");
        whole = false;
    }
    esp_http_client_close(client);
    esp_http_client_cleanup(client);
    if (whole && !answer.stopped) {
        answer.result = DH_TRY_ANSWERED;
    } else {
        answer.result = DH_TRY_NO_ANSWER;
        answer.status = 0;
        answer.len = 0;
        answer.has_length = false;
        answer.length = 0;
    }
    return answer;
}

int hub_http_download_read(hub_download_t *download, uint8_t *out, size_t cap)
{
    if (cap == 0) {
        return 0;
    }
    const int got = esp_http_client_read(download->client, (char *)out, (int)cap);
    if (got > 0) {
        return got;
    }
    // A wait that ran out with nothing read is the only "nothing yet". Anything else, the end
    // of the body included, is the end of this download.
    return got == -ESP_ERR_HTTP_EAGAIN ? 0 : -1;
}

void hub_http_download_close(hub_download_t *download)
{
    if (download == NULL) {
        return;
    }
    esp_http_client_close(download->client);
    esp_http_client_cleanup(download->client);
    esp_log_level_set(TRANSPORT_LOG_TAG, download->transport_level);
    esp_log_level_set(HTTP_CLIENT_LOG_TAG, download->http_client_level);
    free(download);
}
