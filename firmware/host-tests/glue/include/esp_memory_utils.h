/* A stand-in for ESP-IDF's header of this name: see ../README.md. */
#pragma once

#include <stdbool.h>

/* Whether p is in internal memory. Here: whether the stand-in was told it is. */
bool esp_ptr_internal(const void *p);
