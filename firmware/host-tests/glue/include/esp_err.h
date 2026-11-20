/* A stand-in for ESP-IDF's header of this name: see ../README.md. */
#pragma once

typedef int esp_err_t;

#define ESP_OK 0
#define ESP_FAIL (-1)
#define ESP_ERR_INVALID_STATE 0x103
#define ESP_ERR_NOT_FOUND 0x105
#define ESP_ERR_OTA_ROLLBACK_FAILED 0x1505

const char *esp_err_to_name(esp_err_t err);
