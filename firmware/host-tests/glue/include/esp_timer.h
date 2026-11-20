/* A stand-in for ESP-IDF's header of this name: see ../README.md. */
#pragma once

#include <stdint.h>

/* Microseconds since the start: the stand-in's clock, which only the test moves. */
int64_t esp_timer_get_time(void);
