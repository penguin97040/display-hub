/* A stand-in for FreeRTOS's header of this name: see ../../README.md. */
#pragma once

#include "freertos/FreeRTOS.h"

typedef void *TaskHandle_t;
typedef void (*TaskFunction_t)(void *);

/* Doesn't run the task: the test runs it itself (glue_run_task). */
BaseType_t xTaskCreate(TaskFunction_t task, const char *name, uint32_t stack_bytes, void *arg,
                       unsigned priority, TaskHandle_t *out);
/* Moves the stand-in's clock on by the delay. */
void vTaskDelay(TickType_t ticks);
/* The task is over: jumps back to the test. */
void vTaskDelete(TaskHandle_t task);
