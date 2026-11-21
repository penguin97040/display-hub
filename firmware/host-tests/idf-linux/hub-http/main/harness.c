// Drives firmware/main/hub_http.c, built for a computer over ESP-IDF's own HTTP client, esp-tls
// and Mbed TLS, against a stand-in hub (server.py) that misbehaves on purpose.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "esp_log.h"
#include "esp_timer.h"
#include "hub_http.h"

#define BASE 19000
static char hub_pem[4096], other_pem[4096], wrongname_pem[4096];
static uint8_t body[70000];
static int failures, checks;
static int64_t stop_at_ms; // 0: never
static int64_t stop_first_seen_ms;

#include <signal.h>
#include <time.h>
// The Linux build has no esp_timer: a steady clock stands in.
int64_t esp_timer_get_time(void)
{
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (int64_t)t.tv_sec * 1000000 + t.tv_nsec / 1000;
}
static int64_t now_ms(void) { return esp_timer_get_time() / 1000; }
static bool stop(void)
{
    const bool s = stop_at_ms != 0 && now_ms() >= stop_at_ms;
    if (s && stop_first_seen_ms == 0) stop_first_seen_ms = now_ms();
    return s;
}
static void load(const char *dir, const char *name, char *out, size_t cap)
{
    char path[512];
    snprintf(path, sizeof path, "%s/%s.pem", dir, name);
    FILE *f = fopen(path, "rb");
    if (!f) { printf("can't read %s\n", path); exit(2); }
    size_t n = fread(out, 1, cap - 1, f); out[n] = 0; fclose(f);
}
#define CHECK(what, cond) do { checks++; if (!(cond)) { failures++; printf("    FAILED: %s\n", what); } } while (0)

typedef struct {
    const char *name; int port; const char *anchor; const char *post; bool auth;
    int stop_after_ms; size_t ok_cap;
    dh_try_result want; int want_status; bool want_stopped; int min_ms, max_ms; const char *body_has;
    const char *host;
} scenario;

static void run(const scenario *sc)
{
    memset(body, 0, sizeof body);
    const hub_request_t request = {
        .host = sc->host ? sc->host : "127.0.0.1", .port = (uint16_t)(BASE + sc->port), .anchor_pem = sc->anchor,
        .path = "/v1/test", .body = sc->post, .body_len = sc->post ? strlen(sc->post) : 0,
        .authorization = sc->auth ? "Bearer test-token" : NULL,
        .answer = body, .ok_cap = sc->ok_cap ? sc->ok_cap : 4096, .error_cap = 4096, .stop = stop,
    };
    const int64_t t0 = now_ms();
    stop_at_ms = sc->stop_after_ms ? t0 + sc->stop_after_ms : 0;
    stop_first_seen_ms = 0;
    const hub_answer_t a = hub_http_request(&request);
    const int took = (int)(now_ms() - t0);
    printf("%-34s result %d status %d len %u stopped %d took %d ms", sc->name, (int)a.result, a.status, (unsigned)a.len, (int)a.stopped, took);
    if (sc->stop_after_ms && stop_first_seen_ms) printf(" (%d ms after stop)", (int)(now_ms() - stop_first_seen_ms));
    printf("\n");
    CHECK("result", a.result == sc->want);
    CHECK("status", a.status == sc->want_status);
    CHECK("stopped", a.stopped == sc->want_stopped);
    CHECK("not too soon", took >= sc->min_ms);
    CHECK("in time", took <= sc->max_ms);
    if (sc->body_has) CHECK("body", strstr((const char *)body, sc->body_has) != NULL);
    stop_at_ms = 0;
}

static void download_test(void)
{
    const hub_request_t request = {
        .host = "127.0.0.1", .port = BASE + 11, .anchor_pem = hub_pem, .path = "/v1/firmware/x",
        .authorization = "Bearer test-token", .answer = body, .error_cap = 4096, .stop = stop,
    };
    hub_download_t *d = NULL;
    const int64_t t0 = now_ms();
    const hub_download_answer_t a = hub_http_download_open(&request, &d);
    printf("%-34s result %d status %d has_length %d length %u\n", "download: open", (int)a.result, a.status, (int)a.has_length, (unsigned)a.length);
    CHECK("download opened", a.result == DH_TRY_ANSWERED && a.status == 200 && d != NULL && a.has_length && a.length == 102400);
    if (d == NULL) return;
    size_t total = 0; bool right = true; int idle = 0;
    while (total < 102400 && idle < 200) {
        uint8_t piece[512];
        const int got = hub_http_download_read(d, piece, sizeof piece);
        if (got < 0) break;
        if (got == 0) { idle++; continue; }
        idle = 0;
        for (int i = 0; i < got; i++) if (piece[i] != (uint8_t)((total + (size_t)i) & 0xff)) right = false;
        total += (size_t)got;
    }
    uint8_t one;
    const int after = hub_http_download_read(d, &one, 1);
    hub_http_download_close(d);
    printf("%-34s %u bytes, right %d, read after the end %d, took %d ms\n", "download: body", (unsigned)total, (int)right, after, (int)(now_ms() - t0));
    CHECK("download body", total == 102400 && right && after < 0);

    // A download asked of a hub that trickles its headers: bounded like any request.
    hub_request_t slow = request; slow.port = BASE + 6;
    const int64_t t1 = now_ms();
    const hub_download_answer_t b = hub_http_download_open(&slow, &d);
    const int took = (int)(now_ms() - t1);
    printf("%-34s result %d took %d ms\n", "download: headers trickled", (int)b.result, took);
    CHECK("download headers bounded", b.result == DH_TRY_NO_ANSWER && d == NULL && took >= 29000 && took <= 31500);
    // And one stopped during the handshake.
    slow.port = BASE + 4; stop_at_ms = now_ms() + 1500; stop_first_seen_ms = 0;
    const int64_t t2 = now_ms();
    const hub_download_answer_t c = hub_http_download_open(&slow, &d);
    printf("%-34s result %d stopped %d took %d ms\n", "download: stopped in handshake", (int)c.result, (int)c.stopped, (int)(now_ms() - t2));
    CHECK("download stop", c.result == DH_TRY_NO_ANSWER && c.stopped && d == NULL && now_ms() - t2 <= 1700);
    stop_at_ms = 0;
    // An error answer to a download is read like any other.
    slow.port = BASE + 12;
    const hub_download_answer_t e = hub_http_download_open(&slow, &d);
    printf("%-34s result %d status %d len %u\n", "download: an error answer", (int)e.result, e.status, (unsigned)e.len);
    CHECK("download error answer", e.result == DH_TRY_ANSWERED && e.status == 302 && d == NULL && e.len == 17);
}

void app_main(void)
{
    // lwIP has no signals; on a computer a write to a closed socket would end the program.
    signal(SIGPIPE, SIG_IGN);
    const char *dir = getenv("CERTS");
    load(dir, "hub", hub_pem, sizeof hub_pem);
    load(dir, "other", other_pem, sizeof other_pem);
    load(dir, "wrongname", wrongname_pem, sizeof wrongname_pem);
    esp_log_level_set("*", getenv("QUIET") ? ESP_LOG_NONE : ESP_LOG_WARN);
    static const char big_post[] = "{\"pad\":\"0123456789012345678901234567890123456789012345678901234567890123456789\"}";
    const scenario all[] = {
        // name, port, anchor, post, auth, stop, ok_cap, want, status, stopped, min, max, body, host
        {"honest GET (TLS 1.3)", 1, hub_pem, NULL, true, 0, 0, DH_TRY_ANSWERED, 200, false, 0, 3000, "\"first\":\"GET /v1/test HTTP/1.1\",\"body_len\":0,\"has_auth\":true", NULL},
        {"honest POST (TLS 1.3)", 1, hub_pem, big_post, false, 0, 0, DH_TRY_ANSWERED, 200, false, 0, 3000, "\"first\":\"POST /v1/test HTTP/1.1\",\"body_len\":80,\"has_auth\":false", NULL},
        {"honest GET (TLS 1.2)", 13, hub_pem, NULL, true, 0, 0, DH_TRY_ANSWERED, 200, false, 0, 3000, "\"body_len\":0", NULL},
        {"honest POST (TLS 1.2)", 13, hub_pem, big_post, true, 0, 0, DH_TRY_ANSWERED, 200, false, 0, 3000, "\"body_len\":80", NULL},
        {"answer in 7-byte pieces (1.3)", 9, hub_pem, NULL, true, 0, 0, DH_TRY_ANSWERED, 200, false, 0, 9000, "\"split\":true", NULL},
        {"answer in 7-byte pieces (1.2)", 16, hub_pem, NULL, true, 0, 0, DH_TRY_ANSWERED, 200, false, 0, 9000, "\"split\":true", NULL},
        {"a 302 is an answer, not followed", 12, hub_pem, NULL, true, 0, 0, DH_TRY_ANSWERED, 302, false, 0, 3000, "moved", NULL},
        {"an answer longer than allowed", 14, hub_pem, NULL, true, 0, 0, DH_TRY_ANSWERED, 200, false, 0, 3000, NULL, NULL},
        {"another key, same name", 2, hub_pem, NULL, true, 0, 0, DH_TRY_WRONG_HUB, 0, false, 0, 3000, NULL, NULL},
        {"the anchor's key, another name", 3, wrongname_pem, NULL, true, 0, 0, DH_TRY_WRONG_HUB, 0, false, 0, 3000, NULL, NULL},
        {"the second anchor fits", 2, other_pem, NULL, true, 0, 0, DH_TRY_ANSWERED, 200, false, 0, 3000, NULL, NULL},
        {"nothing listening", 99, hub_pem, NULL, true, 0, 0, DH_TRY_NO_ANSWER, 0, false, 0, 3000, NULL, NULL},
        {"a name that doesn't exist", 1, hub_pem, NULL, true, 0, 0, DH_TRY_NO_ANSWER, 0, false, 0, 20000, NULL, "no-such-host.invalid"},
        {"stopped before it starts", 1, hub_pem, NULL, true, -1, 0, DH_TRY_NO_ANSWER, 0, true, 0, 100, NULL, NULL},
        {"handshake trickled", 4, hub_pem, NULL, true, 0, 0, DH_TRY_NO_ANSWER, 0, false, 29000, 31500, NULL, NULL},
        {"handshake trickled, stopped", 4, hub_pem, NULL, true, 2000, 0, DH_TRY_NO_ANSWER, 0, true, 1900, 2200, NULL, NULL},
        {"connected, then silence", 5, hub_pem, NULL, true, 0, 0, DH_TRY_NO_ANSWER, 0, false, 29000, 31500, NULL, NULL},
        {"silence, stopped", 5, hub_pem, NULL, true, 700, 0, DH_TRY_NO_ANSWER, 0, true, 650, 900, NULL, NULL},
        {"headers trickled", 6, hub_pem, NULL, true, 0, 0, DH_TRY_NO_ANSWER, 0, false, 29000, 31500, NULL, NULL},
        {"headers trickled, stopped", 6, hub_pem, NULL, true, 3000, 0, DH_TRY_NO_ANSWER, 0, true, 2900, 3200, NULL, NULL},
        {"headers without end, flooded", 7, hub_pem, NULL, true, 0, 0, DH_TRY_NO_ANSWER, 0, false, 0, 31500, NULL, NULL},
        {"headers flooded, stopped", 7, hub_pem, NULL, true, 1500, 0, DH_TRY_NO_ANSWER, 0, true, 0, 1700, NULL, NULL},
        {"rubbish after the handshake", 8, hub_pem, NULL, true, 0, 0, DH_TRY_NO_ANSWER, 0, false, 0, 31500, NULL, NULL},
        {"rubbish, stopped", 8, hub_pem, NULL, true, 1500, 0, DH_TRY_NO_ANSWER, 0, true, 0, 1700, NULL, NULL},
        {"body trickled", 10, hub_pem, NULL, true, 0, 0, DH_TRY_NO_ANSWER, 0, false, 29000, 33500, NULL, NULL},
        {"body trickled, stopped", 10, hub_pem, NULL, true, 3000, 0, DH_TRY_NO_ANSWER, 0, true, 2900, 3300, NULL, NULL},
        {"body goes quiet", 15, hub_pem, NULL, true, 0, 0, DH_TRY_NO_ANSWER, 0, false, 9500, 11500, NULL, NULL},
    };
    const char *only = getenv("ONLY");
    for (size_t i = 0; i < sizeof all / sizeof all[0]; i++) {
        if (only && !strstr(all[i].name, only)) continue;
        scenario sc = all[i];
        if (sc.stop_after_ms < 0) { stop_at_ms = 1; sc.stop_after_ms = 0; const hub_request_t r = {.host = "127.0.0.1", .port = BASE + 1, .anchor_pem = hub_pem, .path = "/", .answer = body, .ok_cap = 10, .error_cap = 10, .stop = stop};
            const hub_answer_t a = hub_http_request(&r); printf("%-34s result %d stopped %d\n", sc.name, (int)a.result, (int)a.stopped); CHECK("stopped at once", a.stopped && a.result == DH_TRY_NO_ANSWER); stop_at_ms = 0; continue; }
        run(&sc);
        if (strcmp(all[i].name, "an answer longer than allowed") == 0) { checks++; }
    }
    if (!only || strstr("download", only)) download_test();
    printf("%d checks, %d failures\n", checks, failures);
    exit(failures == 0 ? 0 : 1);
}
