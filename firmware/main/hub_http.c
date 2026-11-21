// One HTTPS request to the hub. See hub_http.h.
//
// What was read in ESP-IDF v5.5.6 to write this (paths are under components/):
// - esp_http_client/esp_http_client.c: a redirect is only ever followed inside
//   esp_http_client_perform() (esp_http_check_response, line 1500). This file never calls it: it
//   opens, writes, fetches the headers and reads, so a 3xx status is simply handed back.
//   disable_auto_redirect is set as well.
// - esp-tls/esp_tls_mbedtls.c: the name checked, and sent as SNI, is common_name when it is set
//   (not the address); the anchor is cacert_buf and nothing else unless a bundle or the global
//   store is asked for, and neither is. When the handshake fails it records Mbed TLS's error and,
//   if a certificate was presented, the verification flags (lines 320-330, 507-513).
// - esp-tls/esp_tls.c: a name is looked up with getaddrinfo() (line 242), which is lwIP's.
// - esp_http_client.c again, for a download: esp_http_client_fetch_headers() answers 0 and marks
//   the response chunked when there is no Content-Length above zero (line 1608), so "chunked" is
//   also how a missing length reads; esp_http_client_read() never asks the transport for more
//   than the caller wants or the receive buffer holds, stops at the Content-Length, and answers
//   -ESP_ERR_HTTP_EAGAIN when a wait runs out with nothing read (line 1403). But it reads the
//   connection again and again until it has all it was asked for.
//   esp_http_client_read() takes the timeout for each of its reads of the connection from the
//   client as it is then (esp_http_client.c line 1381), so esp_http_client_set_timeout_ms()
//   after the headers shortens every wait from there on.
//
// WHY THE CONNECTION IS THIS FILE'S OWN (the "link" below), and not esp_http_client's built-in
// one. Whoever answers at a hub address, with or without the hub's key, must not be able to
// hold this task for longer than a fixed time, and a factory reset must be able to end a
// request at once. The built-in connection can do neither:
// - Its handshake runs on a blocking socket (esp_tls.c:467-469 puts it back to blocking after
//   the connect) whose every recv() waits up to the timeout (SO_RCVTIMEO, esp_tls.c:306), and
//   esp_tls_conn_new_sync() looks at the time only when a handshake step comes back wanting
//   more (esp_tls.c:575-598), which a blocking recv() never lets it do (esp_tls_mbedtls.c:302,
//   mbedtls/port/net_sockets.c:343-365). So anything that sends the handshake a byte at a time,
//   each within the timeout, holds it for as long as it likes, and nothing asks stop().
// - esp_http_client_fetch_headers() reads until the headers end, and comes back early only
//   when one read waits its whole timeout or fails (esp_http_client.c:1593-1603). A hub that
//   trickles them, or never ends them, holds it; once the parser has given up it is fed for
//   as long as bytes keep coming.
// - The client's own non-blocking mode (is_async) was looked at and left out. It does bound the
//   handshake, but a failed handshake's TLS error then never reaches the caller: only the
//   blocking connect copies it across (tcp_transport/transport_ssl.c:122-126 against 80-90), so
//   "the certificate isn't the hub's" could no longer be told from "nothing answered". And it
//   leaves fetch_headers() as it is.
// - A timer that shuts the socket down from another task was left out too: the socket can't be
//   got at during a blocking handshake, and it would be a second task acting on a socket this
//   one may be closing.
// esp_http_client takes a transport of the caller's (config.transport, with
// CONFIG_ESP_HTTP_CLIENT_ENABLE_CUSTOM_TRANSPORT; esp_http_client.c:902-906 and 1630-1638), and
// every byte it reads or writes then goes through that transport's functions (lines 1314, 1381,
// 1595, 1655 and 1780). So the link makes the TLS session itself with esp-tls, which is what the
// built-in one does underneath, on a socket that is non-blocking from the start, and looks at
// the time limit and at stop() before every read and write and in every wait:
// - esp_tls_conn_new_async() with cfg.non_block does the lookup and starts the TCP connection
//   on its first call, then waits up to cfg.timeout_ms for it (esp_tls.c:491-524). Each later
//   call runs the handshake as far as it can and comes straight back with 0 when it needs more
//   bytes (esp_tls_mbedtls.c:337-339), 1 when it is done, or -1 with the error recorded in the
//   session's own handle (320-331), which esp_tls_get_error_handle() hands out.
//   That first wait can't be cut into slices: select() empties the sets it was given when it
//   times out (lwip/src/api/sockets.c:2212-2220), and esp-tls keeps them in the session and
//   reuses them, so after one timeout it would never see the connection made. So the first
//   call is given the whole STEP_TIMEOUT_MS, as the built-in connection gave it, and a session
//   still in ESP_TLS_CONNECTING after it is given up. A TCP connection is made or not: there is
//   nothing in it for the other end to trickle.
// - The name lookup inside that first call is lwIP's and blocks too, for as long as lwIP's own
//   resolver tries (a few seconds). Nothing that answers can stretch it.
// - esp_tls_conn_read() on a non-blocking socket answers ESP_TLS_ERR_SSL_WANT_READ when a
//   record isn't all there yet (net_sockets.c:354), and Mbed TLS keeps what it has.
// So: the lookup and the TCP connection are bounded as they always were, and from there to the
// last byte of the answer's headers nothing waits more than LINK_POLL_MS without looking at
// the limit and at stop(). Once the headers are in, the limit is the body's own (read_body(),
// and update.c for a download) and the link only passes bytes on.
//
// TRIED ON A COMPUTER (host-tests/idf-linux/hub-http): this file as it is, built for Linux over
// ESP-IDF's own HTTP client, esp-tls and Mbed TLS, against a stand-in hub. Honest answers with
// TLS 1.2 and 1.3, also cut into 7-byte TCP segments; a certificate from another key, and the
// right key under another name (both "the certificate isn't the hub's"); a handshake sent a
// byte at a time, a connection that says nothing, headers sent a byte at a time, headers that
// never end, and rubbish after the handshake, each as fast as it will go: every one is given
// up at 30 seconds, and within one wait of stop() turning true. On the chip the waits end as
// soon as bytes come (lwIP's select()); the computer's stand-in for select() only looks every
// 50 ms or so, so the times it prints are in steps of that.

#include "hub_http.h"

#include <stdlib.h>
#include <string.h>
#include <sys/select.h>

#include "dh_setup.h"
#include "esp_http_client.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "esp_tls.h"
#include "esp_transport.h"
#include "mbedtls/ssl.h"
#include "mbedtls/x509.h"

static const char *TAG = "hub-http";

// For the TCP connection, each write, each wait for more of the answer's headers, and the
// longest silence allowed while its body is read.
#define STEP_TIMEOUT_MS 10000
// For the whole request, from its start: the connection, the TLS handshake, sending the
// request and reading the answer's headers and body, however slowly any of it comes. One limit
// for all of it, so nothing at a hub address can have more than this of the task's time
// whichever part it drags out. An honest hub on the same network takes a second or two, most
// of it this chip's own share of the handshake. (A download's body is the exception: update.c
// keeps its limits.)
#define ANSWER_TIMEOUT_MS 30000
// The longest the link waits on the socket without looking at the time limit and at stop().
#define LINK_POLL_MS 50
// The hub's response headers are a few hundred bytes; the request's are under 400.
#define RX_BUFFER_BYTES 2048
#define TX_BUFFER_BYTES 1024
// How long one read of an answer's body waits for bytes, and how much one call asks for. As for
// a download (update.c, "How long one read can take"), one call of esp_http_client_read() can
// take (bytes asked for) x (wait) against a hub that sends tiny whole TLS records on purpose, so
// the body can run at most 64 x 50 ms = 3.2 seconds past ANSWER_TIMEOUT_MS. A hub that simply
// goes quiet is given up on after STEP_TIMEOUT_MS without a byte, and an honest answer isn't
// slowed: the bytes of a record already decrypted are handed out without waiting (link_read).
#define ANSWER_STEP_TIMEOUT_MS 50
#define ANSWER_READ_BYTES      64
// How long one read of a download's connection waits for bytes before esp_http_client_read()
// hands back to the caller, which keeps the download's own time limits and looks for a factory
// reset in between. It is short because that call reads the connection again and again until it
// has all it was asked for, and gives up only when one of those reads waits this long for
// nothing: see update.c, "How long one read can take".
#define DOWNLOAD_STEP_TIMEOUT_MS 50
// ESP-IDF's own log tags for the connection. With a non-blocking socket they print an error
// each time part of a TLS record has arrived and the rest hasn't, which is many times a
// download and means nothing. They are turned off while a download, or any other answer's body,
// is read; what goes wrong is logged here and in update.c. (The first is the built-in
// connection's tag. The link logs nothing under it, and it is kept quiet all the same.)
#define TRANSPORT_LOG_TAG   "transport_base"
#define HTTP_CLIENT_LOG_TAG "HTTP_CLIENT"

static bool stopped(const hub_request_t *request)
{
    return request->stop != NULL && request->stop();
}

static int64_t now_ms(void)
{
    return esp_timer_get_time() / 1000;
}

// ---- The link: one TLS session, bounded in time (see the top of this file) ----

typedef struct {
    esp_tls_t *tls;
    esp_tls_cfg_t cfg;
    int fd;
    // While the request is being made and its answer's headers read: whose stop() to ask, and
    // when to give up. NULL once the headers are in (link_release).
    const hub_request_t *request;
    int64_t deadline_ms;
    // Why the link gave up, if it did.
    bool stopped;
    bool timed_out;
    // What esp-tls recorded when the handshake failed.
    esp_err_t tls_last;
    int tls_code;
    int tls_flags;
} link_t;

// False once the request has been stopped or has run out of time: nothing more is read or sent.
static bool link_may_go_on(link_t *link)
{
    if (link->request == NULL) {
        return true;
    }
    if (link->stopped || link->timed_out) {
        return false;
    }
    if (stopped(link->request)) {
        link->stopped = true;
        return false;
    }
    if (now_ms() > link->deadline_ms) {
        link->timed_out = true;
        return false;
    }
    return true;
}

// Waits up to `ms` for the socket to have bytes to read (or room to write). 1 if it has, 0 if
// the wait ran out, -1 if the socket failed.
static int link_wait(const link_t *link, bool for_write, int ms)
{
    fd_set set;
    FD_ZERO(&set);
    FD_SET(link->fd, &set);
    struct timeval wait = {.tv_sec = ms / 1000, .tv_usec = (ms % 1000) * 1000};
    const int ready = select(link->fd + 1, for_write ? NULL : &set, for_write ? &set : NULL, NULL,
                             &wait);
    return ready < 0 ? -1 : ready > 0 ? 1 : 0;
}

static void link_drop(link_t *link)
{
    if (link->tls != NULL) {
        // Frees the session, wiping its keys and buffers (mbedtls_ssl_free), and closes the socket.
        esp_tls_conn_destroy(link->tls);
        link->tls = NULL;
    }
    link->fd = -1;
}

// Looks the name up, makes the TCP connection and does the TLS handshake. 0, or -1 with the
// link saying why.
static int link_connect(esp_transport_handle_t transport, const char *host, int port,
                        int timeout_ms)
{
    link_t *link = esp_transport_get_context_data(transport);
    link_drop(link);
    link->tls = esp_tls_init();
    if (link->tls == NULL) {
        ESP_LOGE(TAG, "out of memory for a TLS session");
        return -1;
    }
    link->cfg.timeout_ms = timeout_ms;
    bool first = true;
    while (true) {
        const int progress =
            esp_tls_conn_new_async(host, (int)strlen(host), port, &link->cfg, link->tls);
        if (progress > 0) {
            // The handshake is done: the other end holds the key of the anchor's certificate.
            if (esp_tls_get_conn_sockfd(link->tls, &link->fd) == ESP_OK && link->fd >= 0) {
                return 0;
            }
            break;
        }
        if (progress < 0) {
            esp_tls_error_handle_t errors = NULL;
            if (esp_tls_get_error_handle(link->tls, &errors) == ESP_OK) {
                link->tls_last =
                    esp_tls_get_and_clear_last_error(errors, &link->tls_code, &link->tls_flags);
            }
            break;
        }
        if (first) {
            // The first call waited for the TCP connection. If that wasn't made in time, no
            // later call would notice it being made (see the top of this file).
            first = false;
            esp_tls_conn_state_t state = ESP_TLS_FAIL;
            if (esp_tls_get_conn_state(link->tls, &state) != ESP_OK ||
                state != ESP_TLS_HANDSHAKE ||
                esp_tls_get_conn_sockfd(link->tls, &link->fd) != ESP_OK || link->fd < 0) {
                link->tls_last = ESP_ERR_ESP_TLS_CONNECTION_TIMEOUT;
                break;
            }
        }
        // The handshake wants more from the other end.
        if (!link_may_go_on(link) || link_wait(link, false, LINK_POLL_MS) < 0) {
            break;
        }
    }
    link_drop(link);
    return -1;
}

// Reads up to `len` bytes of what the hub sent, waiting up to timeout_ms for the first of them.
// As esp_transport_read() answers: the bytes read, 0 if none came in that time, or less than 0
// if the connection has ended or broken, or the request has been stopped or has run out of time.
static int link_read(esp_transport_handle_t transport, char *buffer, int len, int timeout_ms)
{
    link_t *link = esp_transport_get_context_data(transport);
    if (link->tls == NULL || len <= 0) {
        return ERR_TCP_TRANSPORT_CONNECTION_FAILED;
    }
    const int64_t until_ms = now_ms() + (timeout_ms > 0 ? timeout_ms : 0);
    while (true) {
        if (!link_may_go_on(link)) {
            return ERR_TCP_TRANSPORT_CONNECTION_FAILED;
        }
        // Bytes of a record already decrypted are handed out without waiting.
        if (esp_tls_get_bytes_avail(link->tls) <= 0) {
            const int64_t left_ms = until_ms - now_ms();
            const int ready =
                link_wait(link, false,
                          left_ms <= 0 ? 0 : left_ms < LINK_POLL_MS ? (int)left_ms : LINK_POLL_MS);
            if (ready < 0) {
                return ERR_TCP_TRANSPORT_CONNECTION_FAILED;
            }
            if (ready == 0) {
                if (left_ms <= 0) {
                    return ERR_TCP_TRANSPORT_CONNECTION_TIMEOUT;
                }
                continue;
            }
        }
        const ssize_t got = esp_tls_conn_read(link->tls, buffer, (size_t)len);
        if (got > 0) {
            return (int)got;
        }
        if (got == 0) {
            return ERR_TCP_TRANSPORT_CONNECTION_CLOSED_BY_FIN;
        }
        if (got != ESP_TLS_ERR_SSL_WANT_READ && got != ESP_TLS_ERR_SSL_WANT_WRITE) {
            return ERR_TCP_TRANSPORT_CONNECTION_FAILED;
        }
        // Part of a record has come and the rest hasn't: Mbed TLS keeps it. Wait on, while
        // there is time.
        if (now_ms() >= until_ms) {
            return ERR_TCP_TRANSPORT_CONNECTION_TIMEOUT;
        }
    }
}

// Sends up to `len` bytes, waiting up to timeout_ms for room. The bytes sent, or less than 1 if
// none could be: esp_http_client gives the request up then.
static int link_write(esp_transport_handle_t transport, const char *buffer, int len,
                      int timeout_ms)
{
    link_t *link = esp_transport_get_context_data(transport);
    if (link->tls == NULL || len <= 0) {
        return -1;
    }
    const int64_t until_ms = now_ms() + (timeout_ms > 0 ? timeout_ms : 0);
    while (true) {
        if (!link_may_go_on(link)) {
            return -1;
        }
        // Mbed TLS wants the same bytes again after "not yet", which they are.
        const ssize_t sent = esp_tls_conn_write(link->tls, buffer, (size_t)len);
        if (sent > 0) {
            return (int)sent;
        }
        if (sent != ESP_TLS_ERR_SSL_WANT_READ && sent != ESP_TLS_ERR_SSL_WANT_WRITE) {
            return -1;
        }
        if (now_ms() >= until_ms || link_wait(link, true, LINK_POLL_MS) < 0) {
            return 0;
        }
    }
}

// esp_http_client doesn't ask these of a transport it was given, but a transport has them.
static int link_poll_read(esp_transport_handle_t transport, int timeout_ms)
{
    const link_t *link = esp_transport_get_context_data(transport);
    if (link->tls == NULL) {
        return -1;
    }
    const ssize_t waiting = esp_tls_get_bytes_avail(link->tls);
    return waiting > 0 ? (int)waiting : link_wait(link, false, timeout_ms > 0 ? timeout_ms : 0);
}

static int link_poll_write(esp_transport_handle_t transport, int timeout_ms)
{
    const link_t *link = esp_transport_get_context_data(transport);
    return link->tls == NULL ? -1 : link_wait(link, true, timeout_ms > 0 ? timeout_ms : 0);
}

static int link_close(esp_transport_handle_t transport)
{
    link_drop(esp_transport_get_context_data(transport));
    return 0;
}

static int link_destroy(esp_transport_handle_t transport)
{
    link_t *link = esp_transport_get_context_data(transport);
    link_drop(link);
    free(link);
    return 0;
}

// A link for one request, which must end within ANSWER_TIMEOUT_MS of started_ms. NULL if
// there was no memory. Free it with esp_transport_destroy(), after the client that used it.
static esp_transport_handle_t link_new(const hub_request_t *request, int64_t started_ms)
{
    esp_transport_handle_t transport = esp_transport_init();
    link_t *link = calloc(1, sizeof(*link));
    if (transport == NULL || link == NULL) {
        free(link);
        esp_transport_destroy(transport);
        return NULL;
    }
    link->fd = -1;
    link->request = request;
    link->deadline_ms = started_ms + ANSWER_TIMEOUT_MS;
    // Everything else in the configuration stays off (zero): no bundle of public authorities,
    // no global store, no client certificate, no session tickets, no keep-alive.
    // The hub's own certificate is the only trust anchor. (In PEM, esp-tls wants the length to
    // count the NUL at the end.)
    link->cfg.cacert_buf = (const unsigned char *)request->anchor_pem;
    link->cfg.cacert_bytes = (unsigned int)strlen(request->anchor_pem) + 1;
    // The fixed name in every hub certificate, whatever address this connects to.
    link->cfg.common_name = DH_HUB_SERVER_NAME;
    link->cfg.skip_common_name = false;
    // TLS 1.2 or 1.3.
    link->cfg.tls_version = ESP_TLS_VER_ANY;
    // Non-blocking from the start, and left so.
    link->cfg.non_block = true;
    esp_transport_set_context_data(transport, link);
    esp_transport_set_func(transport, link_connect, link_read, link_write, link_close,
                           link_poll_read, link_poll_write, link_destroy);
    return transport;
}

// The answer's headers are in: from here the body's own limits apply (read_body(), or update.c
// for a download), and the request, which the link was only lent, is let go of.
static void link_release(esp_transport_handle_t transport)
{
    link_t *link = esp_transport_get_context_data(transport);
    link->request = NULL;
}

static bool link_stopped(esp_transport_handle_t transport)
{
    const link_t *link = esp_transport_get_context_data(transport);
    return link->stopped;
}

// Why the connection wasn't made: something answered with a certificate the anchor doesn't
// vouch for, or nothing useful answered at all.
static dh_try_result connect_failure(esp_transport_handle_t transport, esp_err_t open_err)
{
    const link_t *link = esp_transport_get_context_data(transport);
    if (link->stopped) {
        return DH_TRY_NO_ANSWER;
    }
    if (link->timed_out) {
        // Whatever answered never proved it was the hub in the time there was.
        ESP_LOGW(TAG, "no connection: nothing answered as a hub in time");
        return DH_TRY_NO_ANSWER;
    }
    // esp-tls records Mbed TLS's error as a positive number. Mbed TLS ends a handshake with one
    // of these two when the certificate isn't vouched for by the anchor or isn't for the hub's
    // name (ssl_tls.c, mbedtls_ssl_verify_certificate). The flags are only there if esp-tls
    // could still see the certificate afterwards.
    const bool mismatch = link->tls_flags != 0 ||
                          link->tls_code == -MBEDTLS_ERR_X509_CERT_VERIFY_FAILED ||
                          link->tls_code == -MBEDTLS_ERR_SSL_BAD_CERTIFICATE;
    ESP_LOGW(TAG, "no connection: %s (%s; TLS error -0x%04x, certificate flags 0x%x)",
             mismatch ? "the certificate isn't the hub's" : "nothing answered as a hub",
             esp_err_to_name(link->tls_last != ESP_OK ? link->tls_last : open_err),
             (unsigned)link->tls_code, (unsigned)link->tls_flags);
    return mismatch ? DH_TRY_WRONG_HUB : DH_TRY_NO_ANSWER;
}

// A request on its way: esp_http_client for the HTTP, over this file's link.
typedef struct {
    esp_http_client_handle_t client;
    esp_transport_handle_t link;
} hub_call_t;

static void close_call(hub_call_t *call)
{
    if (call->client != NULL) {
        esp_http_client_close(call->client);
        esp_http_client_cleanup(call->client);
        call->client = NULL;
    }
    // After the client, which closes the link if it was open.
    esp_transport_destroy(call->link);
    call->link = NULL;
}

// Sets a request up and connects: looks the name up, does the TLS handshake and sends the
// request's headers. False, with *result saying why and *was_stopped set if stop() ended it, if
// no connection was made.
static bool open_request(const hub_request_t *request, int64_t started_ms, hub_call_t *call,
                         dh_try_result *result, bool *was_stopped)
{
    *result = DH_TRY_NO_ANSWER;
    *call = (hub_call_t){0};
    call->link = link_new(request, started_ms);
    if (call->link == NULL) {
        ESP_LOGE(TAG, "couldn't set a request up (out of memory?)");
        return false;
    }
    const esp_http_client_config_t config = {
        .host = request->host,
        .port = request->port,
        .path = request->path,
        .transport_type = HTTP_TRANSPORT_OVER_SSL,
        .method = request->body != NULL ? HTTP_METHOD_POST : HTTP_METHOD_GET,
        // The TLS session is the link's, with the hub's own certificate as the only trust
        // anchor and the hub's fixed name (link_new). The client's built-in connection, which
        // these would otherwise set up, is never used.
        .transport = call->link,
        .use_global_ca_store = false,
        .crt_bundle_attach = NULL,
        .disable_auto_redirect = true,
        .max_authorization_retries = -1,
        .timeout_ms = STEP_TIMEOUT_MS,
        .buffer_size = RX_BUFFER_BYTES,
        .buffer_size_tx = TX_BUFFER_BYTES,
        .keep_alive_enable = false,
    };
    call->client = esp_http_client_init(&config);
    if (call->client == NULL) {
        ESP_LOGE(TAG, "couldn't set a request up (out of memory?)");
        close_call(call);
        return false;
    }

    esp_err_t err = esp_http_client_set_header(call->client, DH_PROTOCOL_HEADER,
                                               DH_PROTOCOL_HEADER_VALUE);
    if (err == ESP_OK && request->authorization != NULL) {
        err = esp_http_client_set_header(call->client, "Authorization", request->authorization);
    }
    if (err == ESP_OK && request->body != NULL) {
        err = esp_http_client_set_header(call->client, "Content-Type", "application/json");
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "couldn't set a request's headers: %s", esp_err_to_name(err));
        close_call(call);
        return false;
    }

    err = esp_http_client_open(call->client,
                               request->body != NULL ? (int)request->body_len : 0);
    if (err != ESP_OK) {
        *result = connect_failure(call->link, err);
        *was_stopped = link_stopped(call->link);
        close_call(call);
        return false;
    }
    // From here the other end has proved it is the hub.
    return true;
}

// Reads the answer's status and headers, within the request's time limit, and then hands the
// time-keeping over to whoever reads the body. The Content-Length (0 for none), or less than 0
// if no answer came; *was_stopped is set if stop() ended the wait.
static int64_t fetch_headers(hub_call_t *call, int *status, bool *was_stopped)
{
    const int64_t content_length = esp_http_client_fetch_headers(call->client);
    *status = esp_http_client_get_status_code(call->client);
    if (link_stopped(call->link)) {
        *was_stopped = true;
    }
    link_release(call->link);
    return content_length;
}

// read_answer() once the connection is ready: reads until the end, or one byte past `cap`.
static bool read_body(esp_http_client_handle_t client, const hub_request_t *request, size_t cap,
                      int64_t started_ms, hub_answer_t *answer)
{
    // When the last bytes came, so a hub that goes quiet is given up on after STEP_TIMEOUT_MS,
    // as it was with a blocking socket, not only at the end of ANSWER_TIMEOUT_MS.
    int64_t heard_ms = now_ms();
    while (answer->len <= cap) {
        if (stopped(request)) {
            answer->stopped = true;
            return false;
        }
        if (now_ms() - started_ms > ANSWER_TIMEOUT_MS) {
            ESP_LOGW(TAG, "the answer took too long");
            return false;
        }
        // A little at a time, so the checks above are made often (see ANSWER_READ_BYTES).
        size_t want = cap + 1 - answer->len;
        if (want > ANSWER_READ_BYTES) {
            want = ANSWER_READ_BYTES;
        }
        const int got =
            esp_http_client_read(client, (char *)request->answer + answer->len, (int)want);
        if (got == -ESP_ERR_HTTP_EAGAIN) {
            // Nothing yet: wait on, unless the hub has been quiet for too long.
            if (now_ms() - heard_ms > STEP_TIMEOUT_MS) {
                ESP_LOGW(TAG, "the answer went quiet");
                return false;
            }
            continue;
        }
        if (got < 0) {
            ESP_LOGW(TAG, "the answer broke off");
            return false;
        }
        if (got == 0) {
            // The end, unless the body is unfinished.
            if (!esp_http_client_is_complete_data_received(client)) {
                ESP_LOGW(TAG, "the answer stopped part-way");
                return false;
            }
            return true;
        }
        answer->len += (size_t)got;
        heard_ms = now_ms();
    }
    return true;
}

// Reads an answer's body into the request's buffer: to its end, or one byte past `cap`. True if
// the whole of it arrived, or as much as was allowed, within ANSWER_TIMEOUT_MS of `started_ms`
// (and at most ANSWER_READ_BYTES x ANSWER_STEP_TIMEOUT_MS past it). The connection is left
// with short waits, so it is only fit to be closed afterwards.
static bool read_answer(esp_http_client_handle_t client, const hub_request_t *request,
                        int64_t content_length, size_t cap, int64_t started_ms,
                        hub_answer_t *answer)
{
    if (!esp_http_client_is_chunked_response(client) && content_length > (int64_t)cap) {
        // Too long by its own account: don't read any of it.
        answer->len = cap + 1;
        return true;
    }
    // Short waits from here, as for a download, so no read can be held open and the time
    // limit is looked at often. (The link's socket never blocks: see the top of this file.)
    if (esp_http_client_set_timeout_ms(client, ANSWER_STEP_TIMEOUT_MS) != ESP_OK) {
        ESP_LOGE(TAG, "couldn't ready the connection for the answer");
        return false;
    }
    const esp_log_level_t transport_level = esp_log_level_get(TRANSPORT_LOG_TAG);
    const esp_log_level_t http_client_level = esp_log_level_get(HTTP_CLIENT_LOG_TAG);
    esp_log_level_set(TRANSPORT_LOG_TAG, ESP_LOG_NONE);
    esp_log_level_set(HTTP_CLIENT_LOG_TAG, ESP_LOG_NONE);
    const bool whole = read_body(client, request, cap, started_ms, answer);
    esp_log_level_set(TRANSPORT_LOG_TAG, transport_level);
    esp_log_level_set(HTTP_CLIENT_LOG_TAG, http_client_level);
    return whole;
}

hub_answer_t hub_http_request(const hub_request_t *request)
{
    hub_answer_t answer = {.result = DH_TRY_NO_ANSWER};
    if (stopped(request)) {
        answer.stopped = true;
        return answer;
    }

    const int64_t started_ms = now_ms();
    hub_call_t call;
    if (!open_request(request, started_ms, &call, &answer.result, &answer.stopped)) {
        return answer;
    }

    bool whole = false;
    if (stopped(request)) {
        answer.stopped = true;
    } else if (request->body != NULL &&
               esp_http_client_write(call.client, request->body, (int)request->body_len) !=
                   (int)request->body_len) {
        answer.stopped = link_stopped(call.link);
        if (!answer.stopped) {
            ESP_LOGW(TAG, "the request couldn't be sent");
        }
    } else {
        int status = 0;
        const int64_t content_length = fetch_headers(&call, &status, &answer.stopped);
        if (answer.stopped) {
            // A factory reset: nothing more is wanted of this request.
        } else if (content_length < 0 || status < 100) {
            ESP_LOGW(TAG, "no answer to the request");
        } else {
            answer.status = status;
            const size_t cap = status == 200 ? request->ok_cap : request->error_cap;
            whole = read_answer(call.client, request, content_length, cap, started_ms, &answer);
        }
    }
    close_call(&call);

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
    hub_call_t call;
    esp_log_level_t transport_level;
    esp_log_level_t http_client_level;
};

hub_download_answer_t hub_http_download_open(const hub_request_t *request, hub_download_t **out)
{
    hub_download_answer_t answer = {.result = DH_TRY_NO_ANSWER};
    *out = NULL;
    if (stopped(request)) {
        answer.stopped = true;
        return answer;
    }

    const int64_t started_ms = now_ms();
    hub_call_t call;
    if (!open_request(request, started_ms, &call, &answer.result, &answer.stopped)) {
        return answer;
    }

    bool keep = false, whole = false;
    if (stopped(request)) {
        answer.stopped = true;
    } else {
        int status = 0;
        const int64_t content_length = fetch_headers(&call, &status, &answer.stopped);
        if (answer.stopped) {
            // A factory reset: nothing more is wanted of this request.
        } else if (content_length < 0 || status < 100) {
            ESP_LOGW(TAG, "no answer to the request");
        } else if (status == 200) {
            // The body is the caller's to read. A response without a length of its own, chunked
            // or not, is one the update core refuses before any of it is read.
            answer.status = status;
            answer.has_length =
                content_length > 0 && !esp_http_client_is_chunked_response(call.client);
            answer.length = answer.has_length ? (uint64_t)content_length : 0;
            // Short waits from here, so no read can be held open and the caller's own time
            // limits are looked at often. (The link's socket never blocks.)
            keep = whole =
                esp_http_client_set_timeout_ms(call.client, DOWNLOAD_STEP_TIMEOUT_MS) == ESP_OK;
            if (!keep) {
                ESP_LOGE(TAG, "couldn't ready the connection for a download");
            }
        } else {
            // An error answer, read like any other.
            hub_answer_t error = {.result = DH_TRY_NO_ANSWER};
            answer.status = status;
            whole = read_answer(call.client, request, content_length, request->error_cap,
                                started_ms, &error);
            answer.len = error.len;
            answer.stopped = error.stopped;
        }
    }

    if (keep) {
        hub_download_t *download = calloc(1, sizeof(*download));
        if (download != NULL) {
            download->call = call;
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
    close_call(&call);
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
    const int got = esp_http_client_read(download->call.client, (char *)out, (int)cap);
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
    close_call(&download->call);
    esp_log_level_set(TRANSPORT_LOG_TAG, download->transport_level);
    esp_log_level_set(HTTP_CLIENT_LOG_TAG, download->http_client_level);
    free(download);
}
