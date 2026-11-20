/* A stand-in for ESP-IDF's header of this name: see ../README.md. */
#pragma once

#include <stddef.h>
#include <stdint.h>

#define MALLOC_CAP_8BIT (1u << 2)
#define MALLOC_CAP_SPIRAM (1u << 10)
#define MALLOC_CAP_INTERNAL (1u << 11)
#define MALLOC_CAP_DEFAULT (1u << 12)

void *heap_caps_calloc(size_t n, size_t size, uint32_t caps);
void heap_caps_free(void *p);
size_t heap_caps_get_free_size(uint32_t caps);
size_t heap_caps_get_minimum_free_size(uint32_t caps);
