// Wi-Fi, as a station only. See wifi.h.

#include "wifi.h"

#include <stdatomic.h>
#include <string.h>

#include "esp_event.h"
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
