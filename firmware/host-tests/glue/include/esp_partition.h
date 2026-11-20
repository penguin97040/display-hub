/* A stand-in for ESP-IDF's header of this name: see ../README.md. */
#pragma once

#include <stdint.h>

typedef enum {
    ESP_PARTITION_SUBTYPE_APP_OTA_MIN = 0x10,
    ESP_PARTITION_SUBTYPE_APP_OTA_0 = 0x10,
    ESP_PARTITION_SUBTYPE_APP_OTA_1 = 0x11,
    ESP_PARTITION_SUBTYPE_APP_OTA_MAX = 0x20,
} esp_partition_subtype_t;

typedef struct {
    esp_partition_subtype_t subtype;
    uint32_t size;
    char label[17];
} esp_partition_t;
