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

#include "hub_http.h"

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

hub_answer_t hub_http_request(const hub_request_t *request)
{
    hub_answer_t answer = {.result = DH_TRY_NO_ANSWER};
    if (stopped(request)) {
        answer.stopped = true;
        return answer;
    }

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
        return answer;
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
        return answer;
    }

    const int64_t started_ms = esp_timer_get_time() / 1000;
    // Looks the name up, connects, does the TLS handshake and sends the request's headers.
    err = esp_http_client_open(client, request->body != NULL ? (int)request->body_len : 0);
    if (err != ESP_OK) {
        answer.result = connect_failure(client, err);
        esp_http_client_cleanup(client);
        return answer;
    }
    // From here the other end has proved it is the hub.

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
            if (!esp_http_client_is_chunked_response(client) && content_length > (int64_t)cap) {
                // Too long by its own account: don't read any of it.
                answer.len = cap + 1;
                whole = true;
            } else {
                // Read until the end, or one byte past what is allowed.
                while (answer.len <= cap) {
                    if (stopped(request)) {
                        answer.stopped = true;
                        break;
                    }
                    if (esp_timer_get_time() / 1000 - started_ms > ANSWER_TIMEOUT_MS) {
                        ESP_LOGW(TAG, "the answer took too long");
                        break;
                    }
                    // A piece at a time, so the checks above are made often.
                    size_t want = cap + 1 - answer.len;
                    if (want > RX_BUFFER_BYTES) {
                        want = RX_BUFFER_BYTES;
                    }
                    const int got = esp_http_client_read(
                        client, (char *)request->answer + answer.len, (int)want);
                    if (got < 0) {
                        ESP_LOGW(TAG, "the answer broke off");
                        break;
                    }
                    if (got == 0) {
                        // The end, or a wait that ran out with the body unfinished.
                        whole = esp_http_client_is_complete_data_received(client);
                        if (!whole) {
                            ESP_LOGW(TAG, "the answer stopped part-way");
                        }
                        break;
                    }
                    answer.len += (size_t)got;
                }
                if (answer.len > cap) {
                    whole = true;
                }
            }
        }
    }
    esp_http_client_close(client);
    esp_http_client_cleanup(client);

    if (whole && !answer.stopped) {
        answer.result = DH_TRY_ANSWERED;
    } else {
        answer.status = 0;
        answer.len = 0;
    }
    return answer;
}
