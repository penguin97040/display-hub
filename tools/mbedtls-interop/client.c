/*
 * A TLS client configured the way ESP-IDF's esp_http_client configures mbedTLS for a display:
 * the hub's own certificate as the only trust anchor (cert_pem), the fixed server name checked
 * (common_name), verification required, and optionally TLS 1.2 only. It sends one HTTP request and
 * prints the status line. Exit status: 0 connected and got HTTP, 2 TLS or verification failure,
 * 1 usage or other error.
 *
 *   client HOST PORT CA_PEM SERVER_NAME tls12|tls13 PATH
 */
#include <stdio.h>
#include <string.h>
#include <mbedtls/ctr_drbg.h>
#include <mbedtls/entropy.h>
#include <mbedtls/error.h>
#include <mbedtls/net_sockets.h>
#include <mbedtls/ssl.h>
#include <mbedtls/x509_crt.h>
#include <psa/crypto.h>

int main(int argc, char **argv) {
    if (argc != 7) {
        fprintf(stderr, "usage: %s HOST PORT CA_PEM SERVER_NAME tls12|tls13 PATH\n", argv[0]);
        return 1;
    }
    const char *host = argv[1], *port = argv[2], *ca = argv[3], *name = argv[4], *version = argv[5], *path = argv[6];
    mbedtls_net_context net;
    mbedtls_ssl_context ssl;
    mbedtls_ssl_config conf;
    mbedtls_x509_crt anchor;
    mbedtls_entropy_context entropy;
    mbedtls_ctr_drbg_context drbg;
    char err[200];
    int ret, status = 1;

    psa_crypto_init();
    mbedtls_net_init(&net);
    mbedtls_ssl_init(&ssl);
    mbedtls_ssl_config_init(&conf);
    mbedtls_x509_crt_init(&anchor);
    mbedtls_entropy_init(&entropy);
    mbedtls_ctr_drbg_init(&drbg);

    if ((ret = mbedtls_ctr_drbg_seed(&drbg, mbedtls_entropy_func, &entropy, NULL, 0)) != 0) goto fail;
    if ((ret = mbedtls_x509_crt_parse_file(&anchor, ca)) != 0) goto fail;
    if ((ret = mbedtls_ssl_config_defaults(&conf, MBEDTLS_SSL_IS_CLIENT, MBEDTLS_SSL_TRANSPORT_STREAM,
                                           MBEDTLS_SSL_PRESET_DEFAULT)) != 0) goto fail;
    mbedtls_ssl_conf_authmode(&conf, MBEDTLS_SSL_VERIFY_REQUIRED);
    mbedtls_ssl_conf_ca_chain(&conf, &anchor, NULL);
    mbedtls_ssl_conf_rng(&conf, mbedtls_ctr_drbg_random, &drbg);
    /* ESP-IDF builds commonly use 4 KiB receive buffers; ask for small records. */
    mbedtls_ssl_conf_max_frag_len(&conf, MBEDTLS_SSL_MAX_FRAG_LEN_4096);
    if (strcmp(version, "tls12") == 0) {
        mbedtls_ssl_conf_max_tls_version(&conf, MBEDTLS_SSL_VERSION_TLS1_2);
    }
    if ((ret = mbedtls_ssl_setup(&ssl, &conf)) != 0) goto fail;
    if ((ret = mbedtls_ssl_set_hostname(&ssl, name)) != 0) goto fail;
    if ((ret = mbedtls_net_connect(&net, host, port, MBEDTLS_NET_PROTO_TCP)) != 0) goto fail;
    mbedtls_ssl_set_bio(&ssl, &net, mbedtls_net_send, mbedtls_net_recv, NULL);

    while ((ret = mbedtls_ssl_handshake(&ssl)) != 0) {
        if (ret != MBEDTLS_ERR_SSL_WANT_READ && ret != MBEDTLS_ERR_SSL_WANT_WRITE) {
            uint32_t flags = mbedtls_ssl_get_verify_result(&ssl);
            if (flags != 0) {
                char info[512];
                mbedtls_x509_crt_verify_info(info, sizeof info, "  ", flags);
                fprintf(stderr, "verification failed:\n%s", info);
            }
            status = 2;
            goto fail;
        }
    }
    printf("handshake: %s, %s\n", mbedtls_ssl_get_version(&ssl), mbedtls_ssl_get_ciphersuite(&ssl));

    char req[512];
    int n = snprintf(req, sizeof req,
                     "GET %s HTTP/1.1\r\nHost: %s\r\nX-Display-Protocol: 1\r\nAuthorization: Bearer dhd_not-a-real-token\r\n"
                     "Connection: close\r\n\r\n", path, name);
    if ((ret = mbedtls_ssl_write(&ssl, (const unsigned char *)req, (size_t)n)) <= 0) goto fail;
    unsigned char buf[4096];
    ret = mbedtls_ssl_read(&ssl, buf, sizeof buf - 1);
    if (ret <= 0) goto fail;
    buf[ret] = 0;
    char *eol = strstr((char *)buf, "\r\n");
    if (eol) *eol = 0;
    printf("response: %s\n", (char *)buf);
    status = strncmp((char *)buf, "HTTP/1.1 ", 9) == 0 ? 0 : 1;
    mbedtls_ssl_close_notify(&ssl);
    goto done;

fail:
    mbedtls_strerror(ret, err, sizeof err);
    fprintf(stderr, "error: %s (-0x%04x)\n", err, (unsigned)-ret);
done:
    mbedtls_net_free(&net);
    mbedtls_ssl_free(&ssl);
    mbedtls_ssl_config_free(&conf);
    mbedtls_x509_crt_free(&anchor);
    mbedtls_ctr_drbg_free(&drbg);
    mbedtls_entropy_free(&entropy);
    return status;
}
