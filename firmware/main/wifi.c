// Wi-Fi, as a station only. See wifi.h.

#include "wifi.h"

#include <stdatomic.h>
#include <stdio.h>
#include <string.h>

#include "esp_event.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_netif.h"
#include "esp_wifi.h"

static const char *TAG = "wifi-link";

static TaskHandle_t s_wake;
static bool s_prepared;
// What wifi_prepare() has done so far, so a call after one that failed part way carries on from
// there: none of these may be made twice.
static esp_netif_t *s_netif;
static esp_event_handler_instance_t s_on_disconnected;
static esp_event_handler_instance_t s_on_got_ip;
static esp_event_handler_instance_t s_on_lost_ip;
static bool s_started;
static atomic_int s_link = WIFI_LINK_DOWN;
static atomic_uint s_drops;
static atomic_uint s_reason;
static atomic_int s_rssi;

// At most this many access points are logged after a failed join: plenty for one network, and
// each record is about 90 bytes.
#define SURVEY_MAX_APS 10
// A survey after each of the first few failed joins, then after every tenth, so a network that
// stays away all night doesn't fill the log (or hold up the hub task) every minute.
#define SURVEY_EVERY_FIRST 3
#define SURVEY_EVERY_LATER 10
// Failed joins since Wi-Fi started or last joined. Only the hub task touches it.
static unsigned s_failed_joins;

// Runs on the system's event task, which has a small stack: record and wake, nothing more.
static void on_event(void *arg, esp_event_base_t base, int32_t id, void *data)
{
    (void)arg;
    const int link = atomic_load(&s_link);
    if (base == WIFI_EVENT && id == WIFI_EVENT_STA_DISCONNECTED) {
        // Only while joining or joined. A disconnection the firmware asked for itself
        // (wifi_give_up, wifi_stop) arrives with the link already marked down, and must not be
        // counted as another failure.
        if (link == WIFI_LINK_DOWN) {
            return;
        }
        const wifi_event_sta_disconnected_t *event = data;
        atomic_store(&s_reason, event != NULL ? event->reason : 0u);
        atomic_store(&s_rssi, event != NULL ? event->rssi : 0);
        atomic_store(&s_link, WIFI_LINK_DOWN);
        atomic_fetch_add(&s_drops, 1u);
    } else if (base == IP_EVENT && id == IP_EVENT_STA_GOT_IP) {
        if (link != WIFI_LINK_JOINING) {
            return;
        }
        atomic_store(&s_link, WIFI_LINK_UP);
    } else if (base == IP_EVENT && id == IP_EVENT_STA_LOST_IP) {
        // The address ran out and wasn't renewed while the display thought it was joined. Count
        // it as a drop and leave the access point, so the link is made again from the start.
        if (link != WIFI_LINK_UP) {
            return;
        }
        atomic_store(&s_reason, 0u);
        atomic_store(&s_rssi, 0);
        atomic_store(&s_link, WIFI_LINK_DOWN);
        atomic_fetch_add(&s_drops, 1u);
        esp_wifi_disconnect();
    } else {
        return;
    }
    if (s_wake != NULL) {
        xTaskNotifyGive(s_wake);
    }
}

esp_err_t wifi_prepare(TaskHandle_t wake)
{
    if (s_prepared) {
        return ESP_OK;
    }
    s_wake = wake;

    // ESP-IDF's own log lines for these name the network, its access point and the addresses
    // the display is given. Keep those out of the console: hub.c says what happened in words
    // that give nothing away.
    esp_log_level_set("wifi", ESP_LOG_WARN);
    esp_log_level_set("esp_netif_handlers", ESP_LOG_WARN);

    esp_err_t err = esp_netif_init();
    if (err == ESP_OK) {
        err = esp_event_loop_create_default();
        // Made by an earlier call that failed further on.
        if (err == ESP_ERR_INVALID_STATE) {
            err = ESP_OK;
        }
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "the network stack didn't start: %s", esp_err_to_name(err));
        return err;
    }
    if (s_netif == NULL) {
        s_netif = esp_netif_create_default_wifi_sta();
        if (s_netif == NULL) {
            ESP_LOGE(TAG, "couldn't make the Wi-Fi network interface");
            return ESP_FAIL;
        }
        // What a router's list of devices shows. Nothing about the owner or the hub.
        esp_netif_set_hostname(s_netif, "display-hub-display");
    }

    const wifi_init_config_t init = WIFI_INIT_CONFIG_DEFAULT();
    err = esp_wifi_init(&init);
    if (err == ESP_OK) {
        // The driver must never write the network's details to flash: a factory reset erases
        // only this firmware's own setup namespace. (CONFIG_ESP_WIFI_NVS_ENABLED is off too.)
        err = esp_wifi_set_storage(WIFI_STORAGE_RAM);
    }
    if (err == ESP_OK) {
        err = esp_wifi_set_mode(WIFI_MODE_STA);
    }
    if (err == ESP_OK && s_on_disconnected == NULL) {
        err = esp_event_handler_instance_register(WIFI_EVENT, WIFI_EVENT_STA_DISCONNECTED,
                                                  on_event, NULL, &s_on_disconnected);
    }
    if (err == ESP_OK && s_on_got_ip == NULL) {
        err = esp_event_handler_instance_register(IP_EVENT, IP_EVENT_STA_GOT_IP, on_event, NULL,
                                                  &s_on_got_ip);
    }
    if (err == ESP_OK && s_on_lost_ip == NULL) {
        err = esp_event_handler_instance_register(IP_EVENT, IP_EVENT_STA_LOST_IP, on_event, NULL,
                                                  &s_on_lost_ip);
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "the Wi-Fi driver didn't start: %s", esp_err_to_name(err));
        return err;
    }
    s_prepared = true;
    return ESP_OK;
}

esp_err_t wifi_start(const dh_setup *setup)
{
    if (!s_prepared || !setup->has_wifi || setup->ssid_len == 0 ||
        setup->ssid_len > DH_SETUP_MAX_SSID_BYTES ||
        setup->password_len > DH_SETUP_MAX_PASSWORD_BYTES) {
        return ESP_ERR_INVALID_STATE;
    }
    // Too big for comfort on a stack, and it holds the password: static, and wiped below.
    static wifi_config_t config;
    memset(&config, 0, sizeof(config));
    _Static_assert(sizeof(config.sta.ssid) >= DH_SETUP_MAX_SSID_BYTES, "an SSID must fit");
    _Static_assert(sizeof(config.sta.password) > DH_SETUP_MAX_PASSWORD_BYTES,
                   "a password and its NUL must fit");
    memcpy(config.sta.ssid, setup->ssid, setup->ssid_len);
    memcpy(config.sta.password, setup->password, setup->password_len);
    // With a password, never join a network that asks for none (or for WEP) under the same
    // name. An empty password means the owner's network is open.
    config.sta.threshold.authmode = setup->password_len > 0 ? WIFI_AUTH_WPA_PSK : WIFI_AUTH_OPEN;
    config.sta.sae_pwe_h2e = WPA3_SAE_PWE_BOTH;
    config.sta.pmf_cfg.capable = true;
    config.sta.pmf_cfg.required = false;
    // Look on every channel and take the strongest access point with that name.
    config.sta.scan_method = WIFI_ALL_CHANNEL_SCAN;
    config.sta.sort_method = WIFI_CONNECT_AP_BY_SIGNAL;

    esp_err_t err = esp_wifi_set_config(WIFI_IF_STA, &config);
    dh_wipe(&config, sizeof(config));
    if (err == ESP_OK) {
        err = esp_wifi_start();
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "Wi-Fi didn't start: %s", esp_err_to_name(err));
        wifi_stop();
        return err;
    }
    s_started = true;
    s_failed_joins = 0;
    atomic_store(&s_link, WIFI_LINK_DOWN);
    return ESP_OK;
}

esp_err_t wifi_join(void)
{
    if (!s_started) {
        return ESP_ERR_INVALID_STATE;
    }
    atomic_store(&s_link, WIFI_LINK_JOINING);
    const esp_err_t err = esp_wifi_connect();
    if (err != ESP_OK) {
        atomic_store(&s_link, WIFI_LINK_DOWN);
        ESP_LOGE(TAG, "couldn't start joining Wi-Fi: %s", esp_err_to_name(err));
    }
    return err;
}

void wifi_give_up(void)
{
    if (!s_started || atomic_load(&s_link) != WIFI_LINK_JOINING) {
        return;
    }
    // Count it here: the driver only reports a disconnection if it had got as far as the
    // access point.
    atomic_store(&s_reason, 0u);
    atomic_store(&s_rssi, 0);
    atomic_store(&s_link, WIFI_LINK_DOWN);
    atomic_fetch_add(&s_drops, 1u);
    esp_wifi_disconnect();
}

void wifi_stop(void)
{
    if (!s_prepared) {
        return;
    }
    // Marked down first, so the disconnection this causes isn't counted as a failed join.
    atomic_store(&s_link, WIFI_LINK_DOWN);
    if (s_started) {
        esp_wifi_disconnect();
        esp_wifi_stop();
        s_started = false;
    }
    // Take the network's name and password out of the driver's memory.
    static wifi_config_t empty;
    memset(&empty, 0, sizeof(empty));
    const esp_err_t err = esp_wifi_set_config(WIFI_IF_STA, &empty);
    if (err != ESP_OK) {
        ESP_LOGW(TAG, "the Wi-Fi details stay in the driver's memory until a restart: %s",
                 esp_err_to_name(err));
    }
    atomic_store(&s_link, WIFI_LINK_DOWN);
}

wifi_link_t wifi_link(void)
{
    return (wifi_link_t)atomic_load(&s_link);
}

uint32_t wifi_drops(void)
{
    return atomic_load(&s_drops);
}

unsigned wifi_last_reason(void)
{
    return atomic_load(&s_reason);
}

int wifi_last_rssi(void)
{
    return atomic_load(&s_rssi);
}

const char *wifi_reason_words(unsigned reason)
{
    switch (reason) {
    case 0:
        return "no answer in time";
    case WIFI_REASON_AUTH_EXPIRE:
        // Also what the driver says when an access point turns the display away (WPA3 or
        // protected management frames it can't agree on, some wrong passwords).
        return "authentication didn't finish: the access point didn't answer, or turned the "
               "display away";
    case WIFI_REASON_4WAY_HANDSHAKE_TIMEOUT:
    case WIFI_REASON_HANDSHAKE_TIMEOUT:
        return "the password handshake failed, often a wrong password";
    case WIFI_REASON_NO_AP_FOUND:
        return "network not found";
    case WIFI_REASON_CONNECTION_FAIL:
        return "connection failed";
    case WIFI_REASON_NO_AP_FOUND_W_COMPATIBLE_SECURITY:
    case WIFI_REASON_NO_AP_FOUND_IN_AUTHMODE_THRESHOLD:
        return "no access point with acceptable security";
    default:
        return "other";
    }
}

static const char *auth_words(wifi_auth_mode_t mode)
{
    switch (mode) {
    case WIFI_AUTH_OPEN:
        return "open";
    case WIFI_AUTH_WEP:
        return "WEP";
    case WIFI_AUTH_WPA_PSK:
        return "WPA-PSK";
    case WIFI_AUTH_WPA2_PSK:
        return "WPA2-PSK";
    case WIFI_AUTH_WPA_WPA2_PSK:
        return "WPA/WPA2-PSK";
    case WIFI_AUTH_WPA2_WPA3_PSK:
        return "WPA2/WPA3 mixed";
    case WIFI_AUTH_WPA3_PSK:
    case WIFI_AUTH_WPA3_EXT_PSK:
    case WIFI_AUTH_WPA3_EXT_PSK_MIXED_MODE:
        return "WPA3-SAE";
    case WIFI_AUTH_OWE:
        return "OWE";
    case WIFI_AUTH_WPA2_ENTERPRISE:
    case WIFI_AUTH_WPA3_ENT_192:
    case WIFI_AUTH_WPA3_ENTERPRISE:
    case WIFI_AUTH_WPA2_WPA3_ENTERPRISE:
    case WIFI_AUTH_WPA_ENTERPRISE:
        return "enterprise";
    default:
        return "other";
    }
}

static const char *cipher_words(wifi_cipher_type_t cipher)
{
    switch (cipher) {
    case WIFI_CIPHER_TYPE_NONE:
        return "none";
    case WIFI_CIPHER_TYPE_WEP40:
    case WIFI_CIPHER_TYPE_WEP104:
        return "WEP";
    case WIFI_CIPHER_TYPE_TKIP:
        return "TKIP";
    case WIFI_CIPHER_TYPE_CCMP:
        return "CCMP";
    case WIFI_CIPHER_TYPE_TKIP_CCMP:
        return "TKIP+CCMP";
    case WIFI_CIPHER_TYPE_GCMP:
        return "GCMP";
    case WIFI_CIPHER_TYPE_GCMP256:
        return "GCMP-256";
    default:
        return "other";
    }
}

// The Wi-Fi standards an access point offers, as "b/g/n/ax" (or "none").
static void phy_words(const wifi_ap_record_t *ap, char *out, size_t size)
{
    size_t at = 0;
    out[0] = '\0';
    const struct {
        bool on;
        const char *name;
    } flags[] = {
        {ap->phy_11b, "b"}, {ap->phy_11g, "g"}, {ap->phy_11n, "n"},
        {ap->phy_11ax, "ax"}, {ap->phy_lr, "LR"},
    };
    for (size_t i = 0; i < sizeof(flags) / sizeof(flags[0]) && at < size; i++) {
        if (flags[i].on) {
            const int n = snprintf(out + at, size - at, "%s%s", at > 0 ? "/" : "", flags[i].name);
            if (n < 0) {
                break;
            }
            at += (size_t)n;
        }
    }
    if (at == 0) {
        snprintf(out, size, "none");
    }
}

static void log_ap(const char *what, const wifi_ap_record_t *ap)
{
    char phy[24];
    phy_words(ap, phy, sizeof(phy));
    ESP_LOGI(TAG, "%s: channel %u, %d dBm, %s, cipher %s, 802.11%s", what, (unsigned)ap->primary,
             (int)ap->rssi, auth_words(ap->authmode), cipher_words(ap->pairwise_cipher), phy);
}

void wifi_log_survey(void)
{
    // A scan while a join is under way is refused by the driver (ESP_ERR_WIFI_STATE), and one
    // while joined would only tell us about the link we have.
    if (!s_started || atomic_load(&s_link) != WIFI_LINK_DOWN) {
        return;
    }
    s_failed_joins++;
    if (s_failed_joins > SURVEY_EVERY_FIRST && s_failed_joins % SURVEY_EVERY_LATER != 0) {
        return;
    }
    // The network's name, from the driver: the hub task wiped its own copy once Wi-Fi started.
    // The driver's config holds the password too, so it is static (as in wifi_start) and wiped
    // as soon as the name has been copied out.
    static wifi_config_t config;
    uint8_t ssid[sizeof(config.sta.ssid) + 1];
    memset(ssid, 0, sizeof(ssid));
    esp_err_t err = esp_wifi_get_config(WIFI_IF_STA, &config);
    if (err == ESP_OK) {
        memcpy(ssid, config.sta.ssid, sizeof(config.sta.ssid));
    }
    dh_wipe(&config, sizeof(config));
    if (err != ESP_OK || ssid[0] == 0) {
        dh_wipe(ssid, sizeof(ssid));
        ESP_LOGW(TAG, "couldn't look for the network: %s",
                 err != ESP_OK ? esp_err_to_name(err) : "no network name in the driver");
        return;
    }

    // Every channel, asking for this name only, so a hidden network answers too. About 13
    // channels at up to 250 ms each.
    wifi_scan_config_t scan;
    memset(&scan, 0, sizeof(scan));
    scan.ssid = ssid;
    scan.scan_type = WIFI_SCAN_TYPE_ACTIVE;
    scan.scan_time.active.min = 100;
    scan.scan_time.active.max = 250;
    // Straight after a join this firmware gave up on, the driver may still be leaving it and
    // refuse a scan for a moment: wait and ask again, a few times.
    err = esp_wifi_scan_start(&scan, true);
    for (int tries = 0; err == ESP_ERR_WIFI_STATE && tries < 3; tries++) {
        vTaskDelay(pdMS_TO_TICKS(500));
        err = esp_wifi_scan_start(&scan, true);
    }
    dh_wipe(ssid, sizeof(ssid));
    if (err != ESP_OK) {
        // Nothing was found, so there is no list to free.
        ESP_LOGW(TAG, "couldn't look for the network: %s", esp_err_to_name(err));
        return;
    }

    uint16_t found = 0;
    esp_wifi_scan_get_ap_num(&found);
    // PSRAM if there is any, so the internal memory is left for Wi-Fi and TLS.
    wifi_ap_record_t *aps = heap_caps_calloc(SURVEY_MAX_APS, sizeof(*aps),
                                             MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
    if (aps == NULL) {
        aps = heap_caps_calloc(SURVEY_MAX_APS, sizeof(*aps), MALLOC_CAP_DEFAULT);
    }
    uint16_t count = SURVEY_MAX_APS;
    // Either call frees the driver's whole list.
    err = aps != NULL ? esp_wifi_scan_get_ap_records(&count, aps) : ESP_ERR_NO_MEM;
    if (err != ESP_OK) {
        esp_wifi_clear_ap_list();
        ESP_LOGW(TAG, "%u access points for this network, but their details couldn't be read: %s",
                 (unsigned)found, esp_err_to_name(err));
    } else if (count == 0) {
        ESP_LOGI(TAG, "no access points for this network found on 2.4 GHz");
    } else {
        unsigned strongest = 0;
        for (uint16_t i = 0; i < count; i++) {
            log_ap("access point for this network", &aps[i]);
            if (aps[i].rssi > aps[strongest].rssi) {
                strongest = i;
            }
        }
        ESP_LOGI(TAG, "%u access points for this network; strongest %d dBm on channel %u",
                 (unsigned)found, (int)aps[strongest].rssi, (unsigned)aps[strongest].primary);
    }
    if (aps != NULL) {
        // Each record holds the network's name and the access point's address.
        dh_wipe(aps, SURVEY_MAX_APS * sizeof(*aps));
        heap_caps_free(aps);
    }
}

void wifi_log_joined(void)
{
    // Holds the network's name and the access point's address: static, and wiped below.
    static wifi_ap_record_t ap;
    s_failed_joins = 0;
    if (esp_wifi_sta_get_ap_info(&ap) == ESP_OK) {
        log_ap("joined an access point", &ap);
    }
    dh_wipe(&ap, sizeof(ap));
}
