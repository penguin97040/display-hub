/* A stand-in for ESP-IDF's header of this name: see ../README.md. */
#pragma once

#include <stdio.h>

/* Nothing is printed, but every line is still checked against its arguments. */
#define DH_GLUE_LOG(tag, ...)        \
    do {                             \
        if (0) {                     \
            (void)(tag);             \
            printf(__VA_ARGS__);     \
        }                            \
    } while (0)

#define ESP_LOGE(tag, ...) DH_GLUE_LOG(tag, __VA_ARGS__)
#define ESP_LOGW(tag, ...) DH_GLUE_LOG(tag, __VA_ARGS__)
#define ESP_LOGI(tag, ...) DH_GLUE_LOG(tag, __VA_ARGS__)
