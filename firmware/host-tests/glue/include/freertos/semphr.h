/* A stand-in for FreeRTOS's header of this name: see ../../README.md. */
#pragma once

#include "freertos/FreeRTOS.h"

typedef struct glue_mutex *SemaphoreHandle_t;

SemaphoreHandle_t xSemaphoreCreateMutex(void);
/* The stand-in has one thread, so a mutex taken twice is a mistake in the code under test. */
BaseType_t xSemaphoreTake(SemaphoreHandle_t mutex, TickType_t wait);
BaseType_t xSemaphoreGive(SemaphoreHandle_t mutex);
