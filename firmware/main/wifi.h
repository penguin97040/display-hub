// Wi-Fi, as a station only: joins the network the display was set up with.
//
// The network's name and password are given to the Wi-Fi driver and kept nowhere else: the
// driver holds them in memory only (never in its own flash storage, which a factory reset
// wouldn't erase), and wifi_stop() takes them out of it again. Nothing here logs the network's
// name, its password, or an address on it.
//
// Only one task (hub.c's) calls these. The driver's events arrive on the system's event task,
// which only records what happened and wakes that task.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "dh_setup.h"
#include "esp_err.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    WIFI_LINK_DOWN,    // not joined, and not trying
    WIFI_LINK_JOINING, // a join is under way
    WIFI_LINK_UP,      // joined, with an address
} wifi_link_t;

// Sets the network stack and the Wi-Fi driver up, once. `wake` is the task to wake (with a task
// notification) whenever the link changes. After a failure it may be called again, and carries on
// from where it stopped.
esp_err_t wifi_prepare(TaskHandle_t wake);

// Starts the radio with the setup's Wi-Fi details. The setup must have them (has_wifi).
esp_err_t wifi_start(const dh_setup *setup);
// Tries to join. The answer comes later: watch wifi_link().
esp_err_t wifi_join(void);
// Gives up a join that is taking too long. It counts as a failed join.
void wifi_give_up(void);
// Leaves the network, stops the radio and takes the details out of the driver.
void wifi_stop(void);

wifi_link_t wifi_link(void);
// How many joins have failed or links dropped since wifi_prepare(), so a caller can tell that
// another one has.
uint32_t wifi_drops(void);
// The driver's reason number for the last one (wifi_err_reason_t), for the log.
unsigned wifi_last_reason(void);

#ifdef __cplusplus
}
#endif
